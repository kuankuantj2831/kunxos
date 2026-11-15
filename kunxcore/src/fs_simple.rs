// SFS v2：写前日志（WAL）简单文件系统。
//
// v1 → v2 变化（设计详见 docs/sfs-v2.md）：
// - 磁盘布局：双超级块（主块 0 + 备份块 1，CRC32）、多块数据位图、
//   256 字节 inode（u64 size + 单/双/三间接，单文件 ≈4.4TB）、环形日志区。
// - 写前日志：所有元数据修改（位图/inode 表/间接块/目录块）先写日志环
//   （DATA 记录 + COMMIT 标记），再 copy-out 到真实位置；崩溃后挂载时
//   重放已提交记录，未提交事务整体丢弃 —— 掉电安全。
// - fsync：`fs_checkpoint` 刷位图 + 更新超级块 + 设备 FLUSH。
// - 磨损均衡：位图 RAM 权威、检查点批量刷盘（热点写放大降 ~512×）、
//   轮转分配提示 + 按分区写计数偏置。
// - 一致性检查：`sfs_check`（fsck 语义）。
//
// 磁盘布局 v2（块大小 4KB，块号从 0 开始）：
//   ```text
//   块 0               超级块（主）
//   块 1               超级块（备份）
//   块 2               inode 位图（128 位）
//   块 3..3+B-1        数据位图（B 块，每块 32768 位）
//   块 3+B..3+B+15     inode 表（256 × 256B = 16 块）
//   块 3+B+8..+J-1     日志环（J 块）
//   块 DATA_START..    数据区
//   ```
//
// 日志记录（每条 2 个环块：头块 + 镜像块；COMMIT 只占 1 个头块）：
//   ```text
//   头块: magic@0(u32) seq@4(u64) target@12(u32) crc@16(u32) flags@20(u32)
//   flags: 1=DATA（target 的完整块镜像在下一块），2=COMMIT（同 seq 事务提交）
//   ```

use crate::blkdev::BLK_SIZE;
use core::sync::atomic::{AtomicU64, Ordering};

// ==================== 类型码 ====================

pub const T_REG: u16 = 1;
pub const T_DIR: u16 = 2;
pub const T_CHR: u16 = 3;
pub const T_BLK: u16 = 4;
pub const T_FIFO: u16 = 5;
pub const T_SYM: u16 = 6;

// ==================== 布局常量 ====================

/// 最大 inode 数。
///
/// 512（2026-09-18 从 256 扩容）：rootfs 文件数涨到 196 后，sfsrace
/// （4 进程 × 12 轮 = 48 文件）+ 守护进程信箱/状态文件数秒内耗尽剩余
/// 60 个 inode，全系统 create 失败（表象：守护进程状态写不进、测试
/// BASELINE FAIL: creat）。imap 块 4096B = 32768 位，512 位远未触顶；
/// inode 表 32 块（128KB 磁盘，无 RAM 常驻成本）。
/// ★ 旧 256-inode 镜像由 parse_sb 的 SB_NUM_INODES 护栏拒绝挂载
/// （布局不相容），需 `rm disk.img && make disk` 重建。
const MAX_INODES: usize = 512;
/// inode 大小（字节）。
const INODE_SIZE: usize = 256;
/// 每个 inode 表块容纳的 inode 数。
const INODES_PER_BLOCK: usize = BLK_SIZE / INODE_SIZE; // 16
/// inode 表块数。
const INODE_BLOCKS: usize = MAX_INODES.div_ceil(INODES_PER_BLOCK); // 16
/// 直接块数量。
const NDIRECT: usize = 24;
/// 间接块指针表容量（单块 4KB / 4 字节指针）。
const NINDIRECT: usize = BLK_SIZE / 4; // 1024
/// 双间接容量（块数）。
const NINDIRECT2: usize = NINDIRECT * NINDIRECT;
/// 三间接容量（块数）。
const NINDIRECT3: usize = NINDIRECT * NINDIRECT * NINDIRECT;
/// 单文件最大块数（24 + 1024 + 1024² + 1024³ ≈ 4.4TB）。
const MAX_BLOCKS: usize = NDIRECT + NINDIRECT + NINDIRECT2 + NINDIRECT3;
/// 文件名最大长度（不含结尾 \0）。
const MAX_NAME: usize = 27;
/// 目录项大小。
const DIRENT_SIZE: usize = 32;
/// 每块位图可跟踪的位数。
const BITS_PER_BLOCK: usize = BLK_SIZE * 8; // 32768
/// 数据位图最大块数：每块可跟踪 32768 个 4KB 数据块，8 块仅覆盖 1GiB。
///
/// 原值 8 的缺陷：>1GiB 卷（`layout()` 对 4GiB 卷算出 ~122 块）或超级块
/// `bmap_blocks` 字段被磁盘损坏放大后，mkfs/mount 的 `for b in 0..bmap_blocks`
/// 循环会 `c.bmap[b]` 索引**越界写穿 .bss 相邻全局**（静默内存损坏）。
/// 扩到 256（覆盖 32GiB）+ parse_sb 字段校验 + bmap 访问越界防护三管齐下。
const MAX_BMAP_BLOCKS: usize = 256;

/// 超级块主/备块号。
const SB0: usize = 0;
const SB1: usize = 1;
/// inode 位图块号。
const IMAP_BLOCK: usize = 2;
/// 数据位图起始块号。
const BMAP_START: usize = 3;

/// 超级块魔数 "_SF2"。
const MAGIC: u32 = 0x5F53_4632;
/// 格式版本（写入超级块 SB_VERSION 偏移处）。
const SFS_VERSION: u32 = 2;
/// 日志记录魔数 "SFSJ"。
const JLOG_MAGIC: u32 = 0x5346_534A;
/// 日志记录类型：DATA。
const JFLAG_DATA: u32 = 1;
/// 日志记录类型：COMMIT。
const JFLAG_COMMIT: u32 = 2;
/// 日志区块数上限/下限。
const JLOG_MAX: u32 = 1024;
const JLOG_MIN: u32 = 64;

// 超级块字段偏移。
const SB_MAGIC: usize = 0;
const SB_VERSION: usize = 4;
const SB_BLK_SIZE: usize = 8;
const SB_TOTAL: usize = 12;
const SB_INODE_BLOCKS: usize = 16;
const SB_BMAP_START: usize = 20;
const SB_BMAP_BLOCKS: usize = 24;
const SB_INODE_START: usize = 28;
const SB_JLOG_START: usize = 32;
const SB_JLOG_BLOCKS: usize = 36;
const SB_DATA_START: usize = 40;
const SB_NUM_INODES: usize = 44;
const SB_ROOT_INO: usize = 48;
const SB_FREE_INODES: usize = 52;
const SB_FREE_BLOCKS: usize = 56;
const SB_FLAGS: usize = 60;
const SB_JLOG_HEAD_SEQ: usize = 64;
const SB_JLOG_HEAD_ABS: usize = 72;
const SB_JLOG_NEXT_ABS: usize = 80;
const SB_INO_HINT: usize = 88;
const SB_BLK_HINT: usize = 92;
const SB_CRC: usize = 96;

// inode 字段偏移。
const I_TYPE: usize = 0;
const I_NLINK: usize = 2;
const I_SIZE: usize = 4;
const I_RDEV: usize = 12;
const I_GEN: usize = 16;
const I_CRC: usize = 20;
const I_DIRECT: usize = 24;
const I_INDIRECT: usize = I_DIRECT + NDIRECT * 4; // 120
const I_DOUBLE: usize = I_INDIRECT + 4; // 124
const I_TRIPLE: usize = I_DOUBLE + 4; // 128
// 归属元数据（占用原保留区 @132..143；旧镜像该区为 0 = 无归属，行为同旧版）。
// 非 root 身份创建的 inode 在创建事务内盖章；root/系统创建的保持无归属。
const I_UID: usize = 132; // u32 所有者
const I_GID: usize = 136; // u32 所属组
const I_MODE: usize = 140; // u16 权限位（rwxrwxrwx 及 setuid 等高位）
const I_OWNF: usize = 142; // u16 标志：bit0 = 归属元数据有效
const OWNF_VALID: u16 = 1;

// 日志头块字段偏移。
const JH_MAGIC: usize = 0;
const JH_SEQ_LO: usize = 4;
const JH_SEQ_HI: usize = 8;
const JH_TARGET: usize = 12;
const JH_CRC: usize = 16;
const JH_FLAGS: usize = 20;

/// 单事务待写镜像上限（写满返回失败，事务回滚）。
const MAX_PENDING: usize = 24;
/// 磨损统计分区粒度（块数，512KB 分区）。
const REGION_BLOCKS: usize = 128;
/// 磨损统计分区数。
const MAX_REGIONS: usize = 512;
/// 同时挂载的 SFS 设备数（磁盘启动：root ramdisk + /data + tmpfs = 3，留 1 余量；
/// 表满时 dev_mut 的「复用 0 号」回退会摧毁在用卷状态，必须保证槽位够用）。
const MAX_DEVS: usize = 4;

// ==================== 字节序列化 ====================

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

// ==================== CRC32（IEEE 802.3，反射多项式 0xEDB88320） ====================

fn crc32_chunk(crc: &mut u32, data: &[u8]) {
    for &b in data {
        *crc ^= b as u32;
        for _ in 0..8 {
            let m = *crc & 1;
            *crc >>= 1;
            if m != 0 {
                *crc ^= 0xEDB8_8320;
            }
        }
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    crc32_chunk(&mut crc, data);
    !crc
}

fn crc32_two(a: &[u8], b: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    crc32_chunk(&mut crc, a);
    crc32_chunk(&mut crc, b);
    !crc
}

// ==================== 块读写 ====================

fn read_block(major: u32, block: usize, buf: &mut [u8]) -> bool {
    crate::blkdev::blk_read(major, block, buf) == BLK_SIZE as i32
}
fn write_block(major: u32, block: usize, buf: &[u8]) -> bool {
    crate::blkdev::blk_write(major, block, buf) == BLK_SIZE as i32
}

// ==================== 全局状态 ====================

/// 每设备运行期状态（位图 RAM 权威副本 + 日志游标 + 磨损提示）。
#[derive(Clone, Copy)]
struct DevState {
    used: bool,
    major: u32,
    // 布局
    total: u32,
    inode_blocks: u32,
    bmap_start: u32,
    bmap_blocks: u32,
    inode_start: u32,
    jlog_start: u32,
    jlog_blocks: u32,
    data_start: u32,
    num_inodes: u32,
    root_ino: u32,
    // 计数（RAM 权威，检查点时刷盘）
    free_inodes: u32,
    free_blocks: u32,
    ino_hint: u32,
    blk_hint: u32,
    // 日志游标（RAM 权威）
    jlog_head_seq: u64,
    jlog_head_abs: u64,
    jlog_next_abs: u64,
    next_seq: u64,
    // 位图
    imap: [u8; BLK_SIZE],
    bmap: [[u8; BLK_SIZE]; MAX_BMAP_BLOCKS],
}

const EMPTY_DEV: DevState = DevState {
    used: false,
    major: 0,
    total: 0,
    inode_blocks: 0,
    bmap_start: 0,
    bmap_blocks: 0,
    inode_start: 0,
    jlog_start: 0,
    jlog_blocks: 0,
    data_start: 0,
    num_inodes: 0,
    root_ino: 0,
    free_inodes: 0,
    free_blocks: 0,
    // ★ ino_hint/next_seq 必须保持 0：EMPTY_DEV 全零才能让 DEVS 数组落 .bss
    // （此前常量 1 把 4×~1MB 的位图数组钉在 .data，kernel.img 多 4.2MB）。
    // 两者只在 mount（从超级块/jlog 装载）与 mkfs（置 1）时初始化，槽位
    // 分配（fs_register）到 mount/mkfs 之间无消费方，0 不会被读到。
    ino_hint: 0,
    blk_hint: 0,
    jlog_head_seq: 0,
    jlog_head_abs: 0,
    jlog_next_abs: 0,
    next_seq: 0,
    imap: [0; BLK_SIZE],
    bmap: [[0; BLK_SIZE]; MAX_BMAP_BLOCKS],
};

static mut DEVS: [DevState; MAX_DEVS] = [EMPTY_DEV; MAX_DEVS];

/// 当前活动事务（深度嵌套；0 = 无事务）。
static mut TXN_DEPTH: usize = 0;
/// 调试开关：置位后 journal_recover 打印扫描明细（测试/核验用，默认关闭）。
static mut JLOG_DEBUG: bool = false;

/// 启用/关闭日志恢复扫描跟踪（用于 sfs_host_test 与崩溃诊断）。
pub fn jlog_set_debug(v: bool) {
    unsafe { JLOG_DEBUG = v; }
}

fn jlog_dbg(msg: &str, a: u64, b: u64, c: u64) {
    unsafe {
        if JLOG_DEBUG {
            crate::uart_send_str("[jlog] ");
            crate::uart_send_str(msg);
            crate::print_dec(a);
            crate::uart_send_str(" ");
            crate::print_dec(b);
            crate::uart_send_str(" ");
            crate::print_dec(c);
            crate::uart_send_str("\n");
        }
    }
}
/// 活动事务所属设备。
static mut TXN_MAJOR: u32 = u32::MAX;

/// 事务待写镜像缓冲（块号 + 完整块镜像）。
static mut PENDING_TARGET: [u32; MAX_PENDING] = [0; MAX_PENDING];
static mut PENDING_IMG: [[u8; BLK_SIZE]; MAX_PENDING] = [[0; BLK_SIZE]; MAX_PENDING];
static mut PENDING_N: usize = 0;

/// 事务开始时的位图快照（abort 时恢复）。
static mut SCRATCH_IMAP: [u8; BLK_SIZE] = [0; BLK_SIZE];
static mut SCRATCH_BMAP: [[u8; BLK_SIZE]; MAX_BMAP_BLOCKS] = [[0; BLK_SIZE]; MAX_BMAP_BLOCKS];
static mut SCRATCH_FREE_INODES: u32 = 0;
static mut SCRATCH_FREE_BLOCKS: u32 = 0;

/// 磨损计数（每 128 块一个分区）。
static mut WEAR: [u32; MAX_REGIONS] = [0; MAX_REGIONS];

fn dev_mut(major: u32) -> &'static mut DevState {
    unsafe {
        for i in 0..MAX_DEVS {
            if DEVS[i].used && DEVS[i].major == major {
                return &mut DEVS[i];
            }
        }
        for i in 0..MAX_DEVS {
            if !DEVS[i].used {
                DEVS[i] = EMPTY_DEV;
                DEVS[i].used = true;
                DEVS[i].major = major;
                return &mut DEVS[i];
            }
        }
        // 表满：**绝不复用真设备槽位** —— 此前复用 0 号会改写 root 卷的
        // major 与布局/日志游标（真实可达：磁盘布局挂 5 个 FS 而 MAX_DEVS=4，
        // 对非 SFS major 直调 SFS 查询即触发）。返回专用哑槽位：写入被
        // 吸收、读取得零值，真设备状态完好。
        let scratch = &mut SCRATCH_DEV;
        *scratch = EMPTY_DEV;
        scratch.used = true;
        scratch.major = major;
        scratch.jlog_blocks = 0;
        scratch
    }
}

/// dev_mut 表满时的哑槽位：隔离误入 SFS 层的非 SFS 设备查询，
/// 防止其状态写进任何真实卷（第 12 轮"摧毁根文件系统"事故的护栏）。
static mut SCRATCH_DEV: DevState = EMPTY_DEV;

fn txn_active(major: u32) -> bool {
    unsafe { TXN_DEPTH > 0 && TXN_MAJOR == major }
}

// ==================== 全局 SFS 锁（并发串行化） ====================

/// 全局 SFS 锁：对 SFS 全部共享状态（DEVS / TXN_* / PENDING_* / SCRATCH_* /
/// WEAR / RAM 位图）的任何访问都必须在持有本锁时进行。
///
/// 为什么必须加锁：syscall 在 EL1 处理期间 IRQ 放开（syscall_dispatch 入口
/// `daifclr #2`），定时器抢占可在任意指令间切换任务；而 SFS 的事务状态与分配
/// 位图是**全局静态**（单事务模型）。两个任务并发进入 FS 时：
///   - 事务交错：A 的 txn_begin 使 TXN_DEPTH=1，B 的 txn_begin 使其变 2，
///     二者共享 PENDING_*；任一提交都会把对方未完成操作的暂存混进日志与
///     copy-out —— inode/块串线、目录项与文件内容张冠李戴。
///   - 分配竞态：inode_alloc / data_block_alloc 并发「读-改-写」位图，出现
///     inode 号重复 / 数据块重复分配。
///   - 事务状态被污染（TXN_MAJOR=u32::MAX 且日志区未初始化 jlog_blocks==0）
///     → jlog_place 对 0 取模 → 除零 panic（实测崩溃）。
///
/// 锁语义：按**持有者 TID** 递归（同任务的嵌套调用 fs_symlink →
/// fs_create → fs_write 只增加深度、不互等），把每个顶层 FS 操作串行化为
/// 原子段。编码（单原子字）：bit 32+ = 获取时所在 CPU 号（仅诊断/来源
/// 记录，不参与持有判定），bit 16..32 = 持有者 tid，低 16 位 = 重入深度。
///
/// ★ 持有身份必须是 tid 而非 (CPU, TID) 双令牌：`pick_next` 无核间亲和，
/// 持锁任务可被抢占迁移到其它核——此后 (a) 迁移走的持有者再重入
/// fs_lock 时按旧 CPU 位判「他人持有」死锁退避；(b) 其 fs_unlock 因
/// `w>>32 != 当前核` 误走「非持有者」兜底而**无条件清零**，嵌套深度
/// 丢失：外层临界区尚未退出锁已被释放，他任务重入 → 并发交错
/// PENDING_*/SCRATCH_* 事务状态与 inode/块分配。两处都以 tid 判定后，
/// 迁移只影响诊断位，不影响正确性。
/// tid 必须与深度同字（此前 tid 存独立变量的两步窗口已由单字编码消除）。
/// 持有者已死亡（tid 不再存活）→ 死锁自愈：任意核 CAS 抢锁接管，继承
/// 「持锁任务死亡后后续任务自然接管」的自愈性。
/// 等待者做**短睡眠轮询退避**（sleep_until 1ms，处于 Blocked 而非 Ready）：
/// 不阻塞调度器、不忙等，且**不存在优先级饿死** —— 把锁让给持锁的低优先级
/// 任务（实时音频任务等优先级高于普通任务时不会被 spin-yield 绕死），
/// 持锁任务被抢占后仍会被再次调度并最终释放（无死锁）。
static FS_LOCKED: AtomicU64 = AtomicU64::new(0);

/// FS 临界区锁（TID 单令牌、可重入）。pub 供 vfs::rename 把「目标打开
/// 检查 → fs_rename」封进同一临界区（锁外检查存在 TOCTOU：检查通过到
/// 取锁之间目标被打开，rename 仍会回收打开中的 inode → fd 与新文件别名）。
pub fn fs_lock() {
    let cpu = crate::smp::cpu_id() as u64;
    let cur_tid = crate::sched_cfs::current_tid() as u64;
    loop {
        let w = FS_LOCKED.load(Ordering::Acquire);
        let holder = (w >> 16) & 0xFFFF;
        if w != 0 && holder == cur_tid {
            // 本任务已持有（含迁移到其它核后重入：tid 才是持有身份）：
            // 重入深度 +1（CAS 循环防与并发解锁交错）。
            if FS_LOCKED
                .compare_exchange(w, w + 1, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            continue;
        }
        if w == 0 || holder == 0 || !crate::sched_cfs::tid_alive(holder as u32) {
            // 空闲，或持有者已死亡（不会再释放）：单 CAS 同时写入 CPU 与
            // tid（无两步窗口）并复位深度。
            if FS_LOCKED
                .compare_exchange(
                    w,
                    (cpu << 32) | (cur_tid << 16) | 1,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return;
            }
        }
        // 他人持有且存活（含迁移中的原持有者）：真互斥。1ms 轮询退避：
        // 等待者进入可调度-阻塞态（Blocked），把 CPU 留给持锁任务（含低
        // 优先级），杜绝 spin-yield 下「高优先级等待者饿死低优先级持锁者」
        // 的活锁；唤醒后重试 CAS。
        crate::sched_cfs::sleep_until(crate::timer::now_us() + 1000);
    }
}

pub fn fs_unlock() {
    let cur_tid = crate::sched_cfs::current_tid() as u64;
    loop {
        let w = FS_LOCKED.load(Ordering::Acquire);
        if w == 0 {
            return;
        }
        let holder = (w >> 16) & 0xFFFF;
        if holder == cur_tid {
            // 持有者本尊（含迁移后：tid 身份与所在核无关）：深度 -1，归零
            // 才真正释放（CAS 循环防与并发重入交错）。
            let new = if w & 0xFFFF <= 1 { 0 } else { w - 1 };
            if FS_LOCKED
                .compare_exchange(w, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            continue;
        }
        if holder == 0 || !crate::sched_cfs::tid_alive(holder as u32) {
            // 防御自愈：持有者已死未释放 → 清零防全局 FS 死锁。仅限死亡
            // 情形——此前对存活持有者也无条件清零，任何非持有者误调都会
            // 撕开互斥（并发入临界区）。
            if FS_LOCKED
                .compare_exchange(w, 0, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            continue;
        }
        // 持有者存活且非本任务：误调，不动锁。
        return;
    }
}

/// 任务死亡时若其持有 FS_LOCKED 则强制放锁（含把事务状态复位到干净基线）。
///
/// 供调度器遏制/kill 路径调用（与 net_cleanup_tid 同源）：仅清「确实属于
/// 死亡 tid」的持有——否则不动，避免撕开对存活持有者的互斥。若只靠
/// fs_lock 的懒自愈，存在「死亡后、槽位 tid 复用前无人来抢锁」的窗口：
/// 新任务复用 tid 后持有者看似存活，全系统 SFS 操作永久关闭。
///
/// 调用方须已屏蔽 IRQ 且不持 FS_LOCKED（kill 路径天然满足）。
pub fn fs_cleanup_tid(tid: u32) {
    let t = tid as u64;
    loop {
        let w = FS_LOCKED.load(Ordering::Acquire);
        if w == 0 {
            return;
        }
        let holder = (w >> 16) & 0xFFFF;
        if holder != t {
            return;
        }
        // 归零即释放。深度/事务状态（PENDING_N/TXN_*）一并丢弃：死者
        // 半截事务不提交，等待者的 txn_begin 以 major 不匹配走回滚。
        if FS_LOCKED
            .compare_exchange(w, 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}

// ==================== 布局计算 ====================

/// 按分区块数求布局：(数据位图块数, inode 表起始, 日志起始, 日志块数, 数据起始)。
fn layout(total: usize) -> (usize, usize, usize, usize, usize) {
    let jlog = (total / 8).clamp(JLOG_MIN as usize, JLOG_MAX as usize);
    let mut bmap = 1usize;
    loop {
        let inode_start = BMAP_START + bmap;
        let jlog_start = inode_start + INODE_BLOCKS;
        let data_start = jlog_start + jlog;
        let ndata = total.saturating_sub(data_start);
        let need = ndata.div_ceil(BITS_PER_BLOCK).max(1);
        if need == bmap {
            // 位图块数超出数组上限：卷太大（或几何不合理）无法挂载——继续返回
            // 未校验的 bmap 会让 mkfs 的 `c.bmap[b]` 越界写穿 .bss。返回哨兵由
            // mkfs 明确拒绝。
            if bmap > MAX_BMAP_BLOCKS {
                return (usize::MAX, 0, 0, 0, 0);
            }
            return (bmap, inode_start, jlog_start, jlog, data_start);
        }
        bmap = need;
    }
}

// ==================== 超级块 ====================

fn write_superblock(major: u32) {
    let c = dev_mut(major);
    let mut sb = [0u8; BLK_SIZE];
    put_u32(&mut sb, SB_MAGIC, MAGIC);
    put_u32(&mut sb, SB_VERSION, SFS_VERSION);
    put_u32(&mut sb, SB_BLK_SIZE, BLK_SIZE as u32);
    put_u32(&mut sb, SB_TOTAL, c.total);
    put_u32(&mut sb, SB_INODE_BLOCKS, c.inode_blocks);
    put_u32(&mut sb, SB_BMAP_START, c.bmap_start);
    put_u32(&mut sb, SB_BMAP_BLOCKS, c.bmap_blocks);
    put_u32(&mut sb, SB_INODE_START, c.inode_start);
    put_u32(&mut sb, SB_JLOG_START, c.jlog_start);
    put_u32(&mut sb, SB_JLOG_BLOCKS, c.jlog_blocks);
    put_u32(&mut sb, SB_DATA_START, c.data_start);
    put_u32(&mut sb, SB_NUM_INODES, c.num_inodes);
    put_u32(&mut sb, SB_ROOT_INO, c.root_ino);
    put_u32(&mut sb, SB_FREE_INODES, c.free_inodes);
    put_u32(&mut sb, SB_FREE_BLOCKS, c.free_blocks);
    put_u32(&mut sb, SB_FLAGS, 0);
    put_u64(&mut sb, SB_JLOG_HEAD_SEQ, c.jlog_head_seq);
    put_u64(&mut sb, SB_JLOG_HEAD_ABS, c.jlog_head_abs);
    put_u64(&mut sb, SB_JLOG_NEXT_ABS, c.jlog_next_abs);
    put_u32(&mut sb, SB_INO_HINT, c.ino_hint);
    put_u32(&mut sb, SB_BLK_HINT, c.blk_hint);
    let crc = crc32(&sb[0..SB_CRC]);
    put_u32(&mut sb, SB_CRC, crc);
    // 先写备份再写主：任一时刻至少有一份完整可用的超级块
    write_block(major, SB1, &sb);
    write_block(major, SB0, &sb);
}

/// 读超级块（主 → 备份回退），校验魔数与 CRC，返回 0；失败 -1。
fn read_sb(major: u32, sb: &mut [u8; BLK_SIZE]) -> bool {
    if read_block(major, SB0, sb) && sb_valid(sb) {
        return true;
    }
    if read_block(major, SB1, sb) && sb_valid(sb) {
        return true;
    }
    false
}

fn sb_valid(sb: &[u8; BLK_SIZE]) -> bool {
    get_u32(sb, SB_MAGIC) == MAGIC && crc32(&sb[0..SB_CRC]) == get_u32(sb, SB_CRC)
}

/// 解析超级块字段到 `c`（须先经 `read_sb` 校验魔数/CRC）。
///
/// ★ 布局字段合法性校验：`bmap_blocks` 超出位图数组上限或磁盘几何自相矛盾时
/// 返回 `false`（挂载方拒绝），避免后续 `c.bmap[b]` 索引**越界写穿 .bss 相邻
/// 全局**（此前直接信任磁盘超级块，一轮坏的超级块即可静默损坏内核内存）。
fn parse_sb(sb: &[u8; BLK_SIZE], c: &mut DevState) -> bool {
    c.total = get_u32(sb, SB_TOTAL);
    c.inode_blocks = get_u32(sb, SB_INODE_BLOCKS);
    c.bmap_start = get_u32(sb, SB_BMAP_START);
    c.bmap_blocks = get_u32(sb, SB_BMAP_BLOCKS);
    c.inode_start = get_u32(sb, SB_INODE_START);
    c.jlog_start = get_u32(sb, SB_JLOG_START);
    c.jlog_blocks = get_u32(sb, SB_JLOG_BLOCKS);
    c.data_start = get_u32(sb, SB_DATA_START);
    c.num_inodes = get_u32(sb, SB_NUM_INODES);
    c.root_ino = get_u32(sb, SB_ROOT_INO);
    c.free_inodes = get_u32(sb, SB_FREE_INODES);
    c.free_blocks = get_u32(sb, SB_FREE_BLOCKS);
    c.ino_hint = get_u32(sb, SB_INO_HINT);
    c.blk_hint = get_u32(sb, SB_BLK_HINT);
    c.jlog_head_seq = get_u64(sb, SB_JLOG_HEAD_SEQ);
    c.jlog_head_abs = get_u64(sb, SB_JLOG_HEAD_ABS);
    c.jlog_next_abs = get_u64(sb, SB_JLOG_NEXT_ABS);
    c.next_seq = c.jlog_head_seq.wrapping_add(1).max(1);
    if c.total == 0
        || c.bmap_blocks == 0
        || c.bmap_blocks as usize > MAX_BMAP_BLOCKS
        || c.data_start as usize >= c.total as usize
        || c.inode_start as usize >= c.data_start as usize
        // ★ inode 容量护栏：镜像的 inode 表几何必须与本内核编译常量一致。
        // 旧 256-inode 镜像的 jlog 紧跟 16 块 inode 表，本内核按 32 块推位
        // 会把 ino 256..511 的读写打进旧日志区（静默损坏），必须拒绝挂载。
        || c.num_inodes as usize != MAX_INODES
    {
        return false;
    }
    true
}

// ==================== 位图 ====================

fn bit_get(bits: &[u8], i: usize) -> bool {
    bits[i / 8] & (1 << (i % 8)) != 0
}
fn bit_set(bits: &mut [u8], i: usize) {
    bits[i / 8] |= 1 << (i % 8);
}
fn bit_clear(bits: &mut [u8], i: usize) {
    bits[i / 8] &= !(1 << (i % 8));
}

/// 数据位图：逻辑块索引（0 = data_start）→ 位。
/// 位图访问越界防护：`idx` 超出位图数组可表达范围（坏超级块 / 越界调用）
/// 一律安全降级——读返回 false（视为未分配），写忽略，杜绝 `c.bmap[b]`
/// 数组越界写穿 .bss。
#[inline]
fn bmap_block_index(idx: usize) -> Option<usize> {
    let b = idx / BITS_PER_BLOCK;
    if b >= MAX_BMAP_BLOCKS {
        None
    } else {
        Some(b)
    }
}

fn bmap_get(c: &DevState, idx: usize) -> bool {
    match bmap_block_index(idx) {
        Some(b) => bit_get(&c.bmap[b], idx % BITS_PER_BLOCK),
        None => false,
    }
}
fn bmap_set(c: &mut DevState, idx: usize) {
    if let Some(b) = bmap_block_index(idx) {
        bit_set(&mut c.bmap[b], idx % BITS_PER_BLOCK);
    }
}
fn bmap_clear(c: &mut DevState, idx: usize) {
    if let Some(b) = bmap_block_index(idx) {
        bit_clear(&mut c.bmap[b], idx % BITS_PER_BLOCK);
    }
}

/// 把数据位图第 `idx` 位所在块的镜像暂存到日志事务。
fn stage_bmap_block(c: &DevState, idx: usize) -> bool {
    // ★ 越界防护：idx 超出位图数组可表达范围时直接跳过（坏卷几何/越界释放），
    // 避免 `&c.bmap[blk_idx]` 读穿 .bss 相邻全局。
    let Some(blk_idx) = bmap_block_index(idx) else { return false };
    stage_block((BMAP_START + blk_idx) as u32, &c.bmap[blk_idx])
}

fn wear_region(block: usize) -> usize {
    (block / REGION_BLOCKS).min(MAX_REGIONS - 1)
}
fn wear_add(block: usize) {
    unsafe {
        let r = wear_region(block);
        WEAR[r] = WEAR[r].wrapping_add(1);
    }
}

/// 磨损感知的轮转数据块分配：从提示位开始扫描，收集最多 8 个空闲候选，
/// 选写计数最低分区的块；返回磁盘块号，失败 -1。
fn data_block_alloc(major: u32) -> i32 {
    let c = dev_mut(major);
    // 位图数组上限钳制：坏超级卷/超大 ndata 时分配范围不超过位图可表达的块数。
    // saturating_sub：磁盘几何自相矛盾（data_start > total）时按 0 处理，防回绕。
    let ndata = c
        .total
        .saturating_sub(c.data_start)
        .min((MAX_BMAP_BLOCKS * BITS_PER_BLOCK) as u32) as usize;
    if ndata == 0 {
        return -1;
    }
    // 轮转 next-fit：从持久化提示位开始，分配/释放都推进提示位，新数据
    // 散布全盘（磨损均衡主机制；磨损计数另由 WEAR 跟踪供诊断）。
    let mut idx = usize::MAX;
    for i in 0..ndata {
        let cand = ((c.blk_hint as usize) + i) % ndata;
        if !bmap_get(c, cand) {
            idx = cand;
            break;
        }
    }
    if idx == usize::MAX {
        return -1;
    }
    // ★ 先写清零、后改位图/日志：此前先置位并 stage 再写零——写零失败会留下
    // 「位图标记已用但块仍残留旧数据」的错乱状态（容量泄漏 + 跨进程残留信息
    // 泄露）。先落零再置位，失败时位图/日志零改动，无需回滚。
    let blk = (c.data_start as usize + idx) as u32;
    let zero = [0u8; BLK_SIZE];
    if !write_block(major, blk as usize, &zero) {
        return -1;
    }
    bmap_set(c, idx);
    c.free_blocks = c.free_blocks.wrapping_sub(1);
    c.blk_hint = ((idx + 1) % ndata) as u32;
    // ★ 位图暂存失败（事务 PENDING 表满）：块已置位但进不了事务——inode 指针
    // 与目录项已进日志并在掉电后重放，而位图分配记录缺失 → 重挂载后该块被
    // 判空闲并分配给新文件 → 跨文件数据踩踏。回滚位图/计数/提示并返回 -1
    // （与 inode_alloc 的 stage 失败处理同款）。
    if !stage_bmap_block(c, idx) {
        bmap_clear(c, idx);
        c.free_blocks = c.free_blocks.wrapping_add(1);
        c.blk_hint = idx as u32;
        return -1;
    }
    wear_add(blk as usize);
    blk as i32
}

fn data_block_free(major: u32, disk_block: u32) {
    let c = dev_mut(major);
    if disk_block < c.data_start {
        return;
    }
    let idx = (disk_block - c.data_start) as usize;
    if idx >= c.total.saturating_sub(c.data_start) as usize {
        return;
    }
    if bmap_get(c, idx) {
        bmap_clear(c, idx);
        c.free_blocks += 1;
        // 释放路径暂存失败属安全方向（掉电后块被误判已用 = 泄漏，不会跨文件
        // 复用），无需回滚已清的位图位。
        stage_bmap_block(c, idx);
    }
}

/// 轮转 inode 分配（跳过 ino 0）。
fn inode_alloc(major: u32) -> i32 {
    let c = dev_mut(major);
    for i in 1..=MAX_INODES {
        let ino = ((c.ino_hint as usize) + i - 1) % MAX_INODES;
        if ino == 0 {
            continue;
        }
        if !bit_get(&c.imap, ino) {
            bit_set(&mut c.imap, ino);
            c.free_inodes = c.free_inodes.wrapping_sub(1);
            c.ino_hint = ((ino + 1) % MAX_INODES) as u32;
            // ★ 位图暂存失败：inode 已置位但进不了事务，提交后磁盘 imap 缺该位
            // （掉电后该 inode 号可被复用 → 目录项悬空），返回失败由调用方回滚。
            if !stage_block(IMAP_BLOCK as u32, &c.imap) {
                return -1;
            }
            return ino as i32;
        }
    }
    -1
}

fn inode_free(major: u32, ino: u32) {
    if ino == 0 || ino as usize >= MAX_INODES {
        return;
    }
    let c = dev_mut(major);
    if bit_get(&c.imap, ino as usize) {
        bit_clear(&mut c.imap, ino as usize);
        c.free_inodes += 1;
        let _ = stage_block(IMAP_BLOCK as u32, &c.imap);
        // 清零 inode 槽（同一事务）：保持「已分配 ↔ 有效 inode」不变量，
        // 避免陈旧 inode 内容被误读（fsck 亦依赖此不变量）
        let zero = [0u8; INODE_SIZE];
        let _ = write_inode(major, ino, &zero);
    }
}

// ==================== 事务暂存 ====================

/// 暂存 `target` 块的新镜像（同块多次修改只保留最终镜像）。
fn stage_block(target: u32, img: &[u8]) -> bool {
    unsafe {
        for i in 0..PENDING_N {
            if PENDING_TARGET[i] == target {
                PENDING_IMG[i].copy_from_slice(img);
                return true;
            }
        }
        if PENDING_N >= MAX_PENDING {
            return false;
        }
        PENDING_TARGET[PENDING_N] = target;
        PENDING_IMG[PENDING_N].copy_from_slice(img);
        PENDING_N += 1;
        true
    }
}

fn pending_find(target: u32) -> Option<&'static [u8; BLK_SIZE]> {
    unsafe {
        for i in 0..PENDING_N {
            if PENDING_TARGET[i] == target {
                return Some(&PENDING_IMG[i]);
            }
        }
    }
    None
}

/// 元数据读：先看事务暂存镜像，再看位图 RAM 缓存，最后读盘。
fn meta_read(major: u32, block: usize, buf: &mut [u8]) -> bool {
    if txn_active(major) {
        if let Some(img) = pending_find(block as u32) {
            buf.copy_from_slice(img);
            return true;
        }
    }
    let c = dev_mut(major);
    if block == IMAP_BLOCK {
        buf.copy_from_slice(&c.imap);
        return true;
    }
    if block >= BMAP_START && block < BMAP_START + c.bmap_blocks as usize {
        buf.copy_from_slice(&c.bmap[block - BMAP_START]);
        return true;
    }
    read_block(major, block, buf)
}

/// 元数据写：位图 → RAM 缓存 + 暂存；其他元数据 → 暂存（提交时 copy-out）。
fn meta_write(major: u32, block: usize, img: &[u8]) -> bool {
    let c = dev_mut(major);
    if block == IMAP_BLOCK {
        c.imap.copy_from_slice(img);
        return stage_block(block as u32, img);
    }
    if block >= BMAP_START && block < BMAP_START + c.bmap_blocks as usize {
        c.bmap[block - BMAP_START].copy_from_slice(img);
        return stage_block(block as u32, img);
    }
    if txn_active(major) {
        stage_block(block as u32, img)
    } else {
        // 无事务：直写（防御性回退）
        write_block(major, block, img)
    }
}

// ==================== 日志（WAL） ====================

/// 为一条 k 块记录在日志环中定位写入位置。
///
/// 本函数**不做检查点**：检查点只在事务边界执行（`txn_begin` 预检与
/// `commit_inner` 提交后），否则中途检查点会把「本事务尚未提交的分配」
/// 刷进磁盘位图，掉电后重放不到这些记录，形成孤儿数据块。
/// `txn_begin` 的预检保证事务全部记录（≤ 2×MAX_PENDING+1 块）在当前环
/// 位置连续放下且不跨环尾，故这里仅做纯定位。失败（防御）返回 -1。
fn jlog_place(major: u32, k: u64) -> i64 {
    let c = dev_mut(major);
    if c.jlog_blocks == 0 {
        // 日志区未初始化（设备未挂载 / 状态被污染）：绝不能在此 `% 0` 除零
        // panic（实测崩溃点）。返回失败，由调用方按「事务未提交」回滚。
        return -1;
    }
    let jb = c.jlog_blocks as u64;
    if c.jlog_next_abs < c.jlog_head_abs {
        // 防御：游标倒退（异常状态）时对齐到 head，避免 used 计算下溢。
        c.jlog_next_abs = c.jlog_head_abs;
    }
    let pos = (c.jlog_next_abs % jb) as usize;
    if pos + k as usize > c.jlog_blocks as usize {
        return -1; // 预检未满足：不应发生（防御）
    }
    c.jlog_next_abs += k;
    pos as i64
}

/// 写一条 DATA 记录（头块 + 镜像块），返回 0 成功；-1 失败（日志区不可用）。
fn jlog_write_data(major: u32, seq: u64, target: u32, img: &[u8; BLK_SIZE]) -> i32 {
    let pos = jlog_place(major, 2);
    if pos < 0 {
        return -1;
    }
    let c = dev_mut(major);
    let hblk = c.jlog_start as usize + pos as usize;
    let mut hdr = [0u8; BLK_SIZE];
    put_u32(&mut hdr, JH_MAGIC, JLOG_MAGIC);
    put_u32(&mut hdr, JH_SEQ_LO, seq as u32);
    put_u32(&mut hdr, JH_SEQ_HI, (seq >> 32) as u32);
    put_u32(&mut hdr, JH_TARGET, target);
    put_u32(&mut hdr, JH_FLAGS, JFLAG_DATA);
    let crc = crc32_two(&hdr[0..16], img);
    put_u32(&mut hdr, JH_CRC, crc);
    if write_block(major, hblk, &hdr) && write_block(major, hblk + 1, img) {
        0
    } else {
        -1
    }
}

/// 写 COMMIT 标记（1 个头块），返回 0 成功；-1 失败。
fn jlog_write_commit(major: u32, seq: u64) -> i32 {
    let pos = jlog_place(major, 1);
    if pos < 0 {
        return -1;
    }
    let c = dev_mut(major);
    let hblk = c.jlog_start as usize + pos as usize;
    let mut hdr = [0u8; BLK_SIZE];
    put_u32(&mut hdr, JH_MAGIC, JLOG_MAGIC);
    put_u32(&mut hdr, JH_SEQ_LO, seq as u32);
    put_u32(&mut hdr, JH_SEQ_HI, (seq >> 32) as u32);
    put_u32(&mut hdr, JH_TARGET, 0);
    put_u32(&mut hdr, JH_FLAGS, JFLAG_COMMIT);
    let crc = crc32(&hdr[0..16]);
    put_u32(&mut hdr, JH_CRC, crc);
    if write_block(major, hblk, &hdr) {
        0
    } else {
        -1
    }
}

/// 检查点：位图刷盘 → 超级块（head 前进 + 计数/提示持久化）→ 设备 FLUSH。
fn checkpoint(major: u32) -> i32 {
    // 存在 copy-out 失败的已提交记录时禁止 head 前移：该记录必须保持可重放，
    // 否则其元数据更新会永久丢失（见 COPYOUT_FAILED 注释）。
    if unsafe { COPYOUT_FAILED[(major as usize) % 16] } {
        crate::uart_send_str("[sfs] checkpoint 被 COPYOUT_FAILED 阻塞 major=");
        crate::print_dec(major as u64);
        crate::uart_send_str("\n");
        return -1;
    }
    let c = dev_mut(major);
    if !write_block(major, IMAP_BLOCK, &c.imap) {
        return -1;
    }
    wear_add(IMAP_BLOCK);
    for b in 0..c.bmap_blocks as usize {
        if !write_block(major, BMAP_START + b, &c.bmap[b]) {
            return -1;
        }
        wear_add(BMAP_START + b);
    }
    c.jlog_head_abs = c.jlog_next_abs;
    if c.next_seq > 1 {
        c.jlog_head_seq = c.next_seq - 1;
    }
    write_superblock(major);
    crate::blkdev::blk_flush(major);
    // 检查点成功：此前 copy-out 失败的事务已随位图/超级块落盘，解除阻塞
    unsafe { COPYOUT_FAILED[(major as usize) % 16] = false };
    0
}

/// 崩溃恢复：扫描日志环，重放所有已提交（有有效 COMMIT）事务的 DATA 记录。
/// 返回重放记录数；同时把 head/next 推进到扫描终点并更新 next_seq。
fn journal_recover(major: u32) -> i32 {
    let c = dev_mut(major);
    let jlog = c.jlog_blocks as u64;
    let mut max_commit = c.jlog_head_seq;
    let mut scan_end = c.jlog_head_abs;

    // 第一遍：找最大的有效 COMMIT 序号（同时确定有效记录终点）。
    // DATA 记录占 2 个环块（头 + 镜像）；扫描必须按记录步进（跳过镜像块），
    // 否则镜像块内容会被当作无效头提前终止扫描，导致其后全部已提交记录
    // 不可见、无法重放（掉电后留下「目录/已落盘但位图未置位」的撕裂状态）。
    let mut i = 0u64;
    let mut last_seq = 0u64;
    while i < jlog {
        let pos = ((c.jlog_head_abs + i) % jlog) as usize;
        let mut hdr = [0u8; BLK_SIZE];
        if !read_block(major, c.jlog_start as usize + pos, &mut hdr) {
            break;
        }
        if get_u32(&hdr, JH_MAGIC) != JLOG_MAGIC {
            break;
        }
        let flags = get_u32(&hdr, JH_FLAGS);
        let seq = get_u64(&hdr, JH_SEQ_LO);
        // 序号单调守卫：物理环序应随 seq 单调（同事务多条记录 seq 相等）。
        // 一旦 seq 回退，说明扫描已越过最新记录、进入上一周期的陈旧区域
        // （旧快照），其后再重放会把陈旧内容覆盖新状态 —— 立即停止。
        if seq < last_seq {
            jlog_dbg("scan1:SEQ-REGRESS-brk pos=", pos as u64, seq, last_seq);
            break;
        }
        last_seq = seq;
        if flags == JFLAG_DATA {
            if pos + 1 >= jlog as usize {
                jlog_dbg("scan1:DATA-straddle-brk pos=", pos as u64, seq, max_commit);
                break;
            }
            let mut payload = [0u8; BLK_SIZE];
            if !read_block(major, c.jlog_start as usize + pos + 1, &mut payload) {
                jlog_dbg("scan1:DATA-readerr pos=", pos as u64, seq, max_commit);
                break;
            }
            if crc32_two(&hdr[0..16], &payload) != get_u32(&hdr, JH_CRC) {
                jlog_dbg("scan1:DATA-crcfail pos=", pos as u64, seq, max_commit);
                break;
            }
            jlog_dbg("scan1:DATA-ok pos=", pos as u64, seq, flags as u64);
            scan_end = c.jlog_head_abs + i + 2;
            i += 2;
        } else if flags == JFLAG_COMMIT {
            if crc32(&hdr[0..16]) != get_u32(&hdr, JH_CRC) {
                jlog_dbg("scan1:COMMIT-crcfail pos=", pos as u64, seq, max_commit);
                break;
            }
            if seq > max_commit {
                max_commit = seq;
            }
            jlog_dbg("scan1:COMMIT-ok pos=", pos as u64, seq, max_commit);
            scan_end = c.jlog_head_abs + i + 1;
            i += 1;
        } else {
            jlog_dbg("scan1:UNKNOWN-brk pos=", pos as u64, seq, flags as u64);
            break;
        }
    }
    jlog_dbg("scan1-done maxc=", max_commit, scan_end, c.jlog_head_seq);

    // 第二遍：按相同步进重放 seq ∈ (head_seq, max_commit] 的 DATA 记录。
    // 与第一遍同样维护 seq 单调守卫：越过最新记录后进入上一周期的陈旧区，
    // 其记录 seq ≤ max_commit 但内容更旧——若照放（仅靠 seq 范围过滤不够，
    // 陈旧 seq 可能落在 (head_seq, max_commit] 内），旧镜像会覆盖同 target
    // 的新状态（宿主掉电测试 k≈106 复现：旧 inode 表覆盖新表，已分配的
    // inode 变无效）。必须在回退处停止，与第一遍的扫描终点保持一致。
    let mut applied = 0u32;
    let mut i = 0u64;
    let mut last_seq = 0u64;
    let mut replay_failed = false;
    while i < jlog {
        let pos = ((c.jlog_head_abs + i) % jlog) as usize;
        let mut hdr = [0u8; BLK_SIZE];
        if !read_block(major, c.jlog_start as usize + pos, &mut hdr) {
            break;
        }
        if get_u32(&hdr, JH_MAGIC) != JLOG_MAGIC {
            break;
        }
        let flags = get_u32(&hdr, JH_FLAGS);
        let seq = get_u64(&hdr, JH_SEQ_LO);
        if seq < last_seq {
            jlog_dbg("replay:SEQ-REGRESS-brk pos=", pos as u64, seq, last_seq);
            break;
        }
        last_seq = seq;
        if flags == JFLAG_DATA {
            if pos + 1 >= jlog as usize {
                break;
            }
            let target = get_u32(&hdr, JH_TARGET) as usize;
            if seq > c.jlog_head_seq && seq <= max_commit && target < c.total as usize {
                let mut payload = [0u8; BLK_SIZE];
                if read_block(major, c.jlog_start as usize + pos + 1, &mut payload) {
                    if write_block(major, target, &payload) {
                        wear_add(target);
                        applied += 1;
                        jlog_dbg("replay:DATA-ok seq=", seq, target as u64, applied as u64);
                    } else {
                        // 重放写失败（掉电/写错误）：必须立刻停止。若继续推进
                        // head/next，未重放记录会被随后新事务覆盖，元数据修改
                        // 永久丢失（孤儿块/悬空目录）。head 保持不动、next 停在
                        // 失败记录处、挂载返回失败；下一次挂载重扫重放。
                        replay_failed = true;
                        jlog_dbg("replay:DATA-writefail seq=", seq, target as u64, applied as u64);
                        break;
                    }
                } else {
                    // ★ 镜像块读失败：此前仅记日志并继续，随后 head 直接推进到
                    // 扫描终点——已提交的 DATA 记录被**静默跳过**，其元数据修改
                    // 永久丢失（孤儿块/悬空目录项）。与写失败同等对待：立即停止，
                    // head/next 保持不动、挂载返回失败，下一次挂载重扫重放。
                    replay_failed = true;
                    jlog_dbg("replay:DATA-readerr seq=", seq, target as u64, applied as u64);
                    break;
                }
            } else {
                jlog_dbg("replay:DATA-skip seq=", seq, target as u64, max_commit);
            }
            i += 2;
        } else if flags == JFLAG_COMMIT {
            i += 1;
        } else {
            break;
        }
    }

    if replay_failed {
        c.jlog_next_abs = c.jlog_head_abs + i;
        return -1;
    }

    // 复位日志游标到扫描终点（已提交记录已重放；未提交记录被整体丢弃，
    // 位于新 head 之前，不再被扫描）
    c.jlog_head_abs = scan_end;
    c.jlog_next_abs = scan_end;
    c.jlog_head_seq = max_commit;
    c.next_seq = max_commit.wrapping_add(1).max(1);
    applied as i32
}

// ==================== 事务接口 ====================

/// 单事务最大日志块数（n 条 DATA × 2 块 + 1 条 COMMIT）。
const MAX_TXN_JLOG_BLOCKS: usize = MAX_PENDING * 2 + 1; // 49

/// 事务 copy-out 失败标记（按 major）：commit 已写入日志但元数据块落盘失败。
/// 置位期间 checkpoint **不得前移 head** —— 否则该记录永久失去重放机会，
/// 重启后目录项/inode 更新丢失而 RAM 位图已放行这些块（跨文件数据踩踏）。
/// 位图镜像本身以 RAM 为权威、由检查点批量刷盘，故阻塞检查点即可让该记录
/// 在下次挂载时经 journal_recover 重放，恢复一致。
static mut COPYOUT_FAILED: [bool; 16] = [false; 16];

fn txn_begin(major: u32) {
    unsafe {
        if TXN_DEPTH == 0 {
            // 事务边界预检：此刻上一事务已提交（或回滚），所有 RAM 元数据
            // 均为「已提交」状态，检查点安全。若本事务的最大记录数放不下
            // （空间不足或会跨环尾），先在此检查点腾出空间，保证 jlog_place
            // 只用纯定位，绝不在事务中途触发检查点。
            let c = dev_mut(major);
            if c.jlog_blocks > 0 {
                let used = c.jlog_next_abs.saturating_sub(c.jlog_head_abs);
                let pos = (c.jlog_next_abs % c.jlog_blocks as u64) as usize;
                let need = MAX_TXN_JLOG_BLOCKS as u64;
                if used + need > c.jlog_blocks as u64
                    || pos as u64 + need > c.jlog_blocks as u64
                {
                    let _ = checkpoint(major);
                    // 检查点后 head=next；若此时仍位于环尾附近（后续记录会跨
                    // 环尾触发 jlog_place 防御失败使写事务整体失败），把
                    // head/next 一起跳过尾区推进到环首。跳过的是上一周期的
                    // 陈旧记录（seq ≤ head_seq，恢复扫描按序号过滤），不会
                    // 误重放；跳过后的空隙在下次扫描时作为「陈旧区」越过。
                    // ★ 跳尾必须随检查点持久化到超级块：若只改 RAM，掉电后
                    // sb 的 head 仍停在环尾，而新事务写在环首——恢复扫描从
                    // head 起遇 INVALID 即停，环首的已提交事务永久不可见，
                    // 其位图镜像无法重放（inode 表已 copy-out、imap 缺位的
                    // 撕裂状态，宿主掉电测试 k≈88 起复现）。
                    let c = dev_mut(major);
                    let pos = (c.jlog_next_abs % c.jlog_blocks as u64) as usize;
                    if pos != 0 && pos as u64 + need > c.jlog_blocks as u64 {
                        c.jlog_next_abs += c.jlog_blocks as u64 - pos as u64;
                        c.jlog_head_abs = c.jlog_next_abs;
                        write_superblock(major);
                    }
                }
            }
            TXN_MAJOR = major;
            PENDING_N = 0;
            snapshot(major);
        } else if TXN_MAJOR != major {
            // 跨设备嵌套（罕见）：先提交外层，再开新事务
            let _ = commit_inner(TXN_MAJOR);
            TXN_MAJOR = major;
            PENDING_N = 0;
            snapshot(major);
        }
        TXN_DEPTH += 1;
    }
}

unsafe fn snapshot(major: u32) {
    let c = dev_mut(major);
    SCRATCH_FREE_INODES = c.free_inodes;
    SCRATCH_FREE_BLOCKS = c.free_blocks;
    SCRATCH_IMAP.copy_from_slice(&c.imap);
    for b in 0..c.bmap_blocks as usize {
        SCRATCH_BMAP[b].copy_from_slice(&c.bmap[b]);
    }
}

fn txn_abort(major: u32) {
    unsafe {
        if TXN_DEPTH == 0 || TXN_MAJOR != major {
            return;
        }
        TXN_DEPTH -= 1;
        if TXN_DEPTH == 0 {
            // 恢复位图与计数（inode 表/目录/间接块未 copy-out，丢弃暂存即可）
            let c = dev_mut(major);
            c.free_inodes = SCRATCH_FREE_INODES;
            c.free_blocks = SCRATCH_FREE_BLOCKS;
            c.imap.copy_from_slice(&SCRATCH_IMAP);
            for b in 0..c.bmap_blocks as usize {
                c.bmap[b].copy_from_slice(&SCRATCH_BMAP[b]);
            }
            PENDING_N = 0;
            TXN_MAJOR = u32::MAX;
        }
    }
}

fn txn_commit(major: u32) -> i32 {
    unsafe {
        if TXN_DEPTH == 0 || TXN_MAJOR != major {
            return -1;
        }        TXN_DEPTH -= 1;
        if TXN_DEPTH == 0 {
            return commit_inner(major);
        }
    }
    0
}

fn commit_inner(major: u32) -> i32 {
    let n = unsafe { PENDING_N };
    if n == 0 {
        unsafe {
            TXN_MAJOR = u32::MAX;
        }
        return 0;
    }
    // 0. 日志区未就绪（设备未挂载 / 事务状态被污染）时绝不动日志：按
    //    「未提交 + 回滚」处理，避免 jlog_place 对 jlog_blocks==0 除零 panic。
    if dev_mut(major).jlog_blocks == 0 {
        unsafe {
            let c = dev_mut(major);
            c.free_inodes = SCRATCH_FREE_INODES;
            c.free_blocks = SCRATCH_FREE_BLOCKS;
            c.imap.copy_from_slice(&SCRATCH_IMAP);
            for b in 0..c.bmap_blocks as usize {
                c.bmap[b].copy_from_slice(&SCRATCH_BMAP[b]);
            }
            PENDING_N = 0;
            TXN_MAJOR = u32::MAX;
        }
        return -1;
    }
    let seq = dev_mut(major).next_seq;
    // 1. 日志 DATA 记录
    let mut ok = true;
    for i in 0..n {
        let t = unsafe { PENDING_TARGET[i] };
        let img = unsafe { &PENDING_IMG[i] };
        if jlog_write_data(major, seq, t, img) != 0 {
            ok = false;
        }
    }
    // 2. COMMIT 标记（必须持久化；失败则事务未提交）
    if jlog_write_commit(major, seq) != 0 {
        ok = false;
    }
    if !ok {
        // 日志写失败（设备错误/掉电）：本事务未提交，**不得 copy-out**，
        // 否则会出现「元数据已落盘但位图未重放」的撕裂状态。
        // 立即回滚 RAM 位图/计数到事务开始快照：泄漏的分配位若残留，
        // 会被后续成功事务的位图记录合并进磁盘（重放后成为无处引用的
        // 孤儿数据块）。回滚后下一次分配会复用这些块（磁盘上仍为清零）。
        unsafe {
            let c = dev_mut(major);
            c.free_inodes = SCRATCH_FREE_INODES;
            c.free_blocks = SCRATCH_FREE_BLOCKS;
            c.imap.copy_from_slice(&SCRATCH_IMAP);
            for b in 0..c.bmap_blocks as usize {
                c.bmap[b].copy_from_slice(&SCRATCH_BMAP[b]);
            }
            PENDING_N = 0;
            TXN_MAJOR = u32::MAX;
        }
        return -1;
    }
    dev_mut(major).next_seq = seq.wrapping_add(1);
    // 3. copy-out 非位图镜像（位图保持 RAM 权威，检查点批量刷盘）
    let bmap_end = BMAP_START + dev_mut(major).bmap_blocks as usize;
    let mut copy_ok = true;
    for i in 0..n {
        let t = unsafe { PENDING_TARGET[i] } as usize;
        if t == IMAP_BLOCK || (t >= BMAP_START && t < bmap_end) {
            continue;
        }
        let img = unsafe { &PENDING_IMG[i] };
        if write_block(major, t, img) {
            wear_add(t);
        } else {
            copy_ok = false;
        }
    }
    if !copy_ok {
        // 元数据 copy-out 失败：日志记录保持可重放（head 不前移，见
        // COPYOUT_FAILED），向调用方报错；RAM 状态保持本事务结果（与磁盘
        // 暂不一致，由重放/后续成功的检查点收敛）。
        crate::uart_send_str("[sfs] copy-out 失败 major=");
        crate::print_dec(major as u64);
        crate::uart_send_str(" blk=");
        crate::print_dec(unsafe { PENDING_TARGET[0] } as u64);
        crate::uart_send_str("\n");
        let mi = (major as usize) % 16;
        unsafe { COPYOUT_FAILED[mi] = true };
        unsafe {
            PENDING_N = 0;
            TXN_MAJOR = u32::MAX;
        }
        return -1;
    }
    // 4. 环使用过半则预检查点（减少日志写爆概率）
    let used = dev_mut(major).jlog_next_abs - dev_mut(major).jlog_head_abs;
    if used >= (dev_mut(major).jlog_blocks as u64) * 3 / 4 {
        let _ = checkpoint(major);
    }
    unsafe {
        PENDING_N = 0;
        TXN_MAJOR = u32::MAX;
    }
    0
}

// ==================== inode ====================

fn inode_block(c: &DevState, ino: u32) -> usize {
    c.inode_start as usize + ino as usize / INODES_PER_BLOCK
}
fn inode_off(ino: u32) -> usize {
    (ino as usize % INODES_PER_BLOCK) * INODE_SIZE
}

fn inode_crc(inode: &[u8; INODE_SIZE]) -> u32 {
    let mut tmp = *inode;
    tmp[I_CRC..I_CRC + 4].fill(0);
    crc32(&tmp)
}

fn inode_fix_crc(inode: &mut [u8; INODE_SIZE]) {
    let c = inode_crc(inode);
    put_u32(inode, I_CRC, c);
}

fn read_inode(major: u32, ino: u32, inode: &mut [u8; INODE_SIZE]) -> bool {
    if ino as usize >= MAX_INODES {
        return false;
    }
    let c = dev_mut(major);
    let mut buf = [0u8; BLK_SIZE];
    if !meta_read(major, inode_block(c, ino), &mut buf) {
        return false;
    }
    inode.copy_from_slice(&buf[inode_off(ino)..inode_off(ino) + INODE_SIZE]);
    // 校验 CRC（未分配的零槽 CRC 必然不匹配，调用方只读已分配 inode）
    inode_crc(inode) == get_u32(inode, I_CRC)
}

fn write_inode(major: u32, ino: u32, inode: &[u8; INODE_SIZE]) -> bool {
    if ino as usize >= MAX_INODES {
        return false;
    }
    let mut tmp = *inode;
    inode_fix_crc(&mut tmp);
    let c = dev_mut(major);
    let mut buf = [0u8; BLK_SIZE];
    if !meta_read(major, inode_block(c, ino), &mut buf) {
        return false;
    }
    let off = inode_off(ino);
    buf[off..off + INODE_SIZE].copy_from_slice(&tmp);
    meta_write(major, inode_block(c, ino), &buf)
}

// inode 字段访问

fn inode_type(inode: &[u8; INODE_SIZE]) -> u16 {
    get_u16(inode, I_TYPE)
}
fn inode_nlink(inode: &[u8; INODE_SIZE]) -> u16 {
    get_u16(inode, I_NLINK)
}
fn inode_size(inode: &[u8; INODE_SIZE]) -> u64 {
    get_u64(inode, I_SIZE)
}
fn inode_rdev(inode: &[u8; INODE_SIZE]) -> u32 {
    get_u32(inode, I_RDEV)
}
fn inode_gen(inode: &[u8; INODE_SIZE]) -> u32 {
    get_u32(inode, I_GEN)
}
fn inode_direct(inode: &[u8; INODE_SIZE], i: usize) -> u32 {
    get_u32(inode, I_DIRECT + i * 4)
}
fn inode_indirect(inode: &[u8; INODE_SIZE]) -> u32 {
    get_u32(inode, I_INDIRECT)
}
fn inode_double(inode: &[u8; INODE_SIZE]) -> u32 {
    get_u32(inode, I_DOUBLE)
}
fn inode_triple(inode: &[u8; INODE_SIZE]) -> u32 {
    get_u32(inode, I_TRIPLE)
}

fn inode_set_type(inode: &mut [u8; INODE_SIZE], v: u16) {
    put_u16(inode, I_TYPE, v);
}
fn inode_set_nlink(inode: &mut [u8; INODE_SIZE], v: u16) {
    put_u16(inode, I_NLINK, v);
}
fn inode_set_size(inode: &mut [u8; INODE_SIZE], v: u64) {
    put_u64(inode, I_SIZE, v);
}
fn inode_set_rdev(inode: &mut [u8; INODE_SIZE], v: u32) {
    put_u32(inode, I_RDEV, v);
}
fn inode_set_gen(inode: &mut [u8; INODE_SIZE], v: u32) {
    put_u32(inode, I_GEN, v);
}
fn inode_set_direct(inode: &mut [u8; INODE_SIZE], i: usize, v: u32) {
    put_u32(inode, I_DIRECT + i * 4, v);
}
fn inode_set_indirect(inode: &mut [u8; INODE_SIZE], v: u32) {
    put_u32(inode, I_INDIRECT, v);
}
fn inode_set_double(inode: &mut [u8; INODE_SIZE], v: u32) {
    put_u32(inode, I_DOUBLE, v);
}
fn inode_set_triple(inode: &mut [u8; INODE_SIZE], v: u32) {
    put_u32(inode, I_TRIPLE, v);
}

// ==================== 块映射 ====================

/// 数据区块号合法性：必须落在 `[data_start, total)` 且在位图可表达范围内。
///
/// 磁盘 inode（CRC 之外的任意字段都可被伪造镜像篡改）中的直接/间接指针
/// 一律先过本检查再解引用：指向元数据区（IMAP/BMAP/日志/超级块）的指针
/// 会把 RAM 权威位图当文件数据读出（泄露）、或把文件数据经 meta_write
/// 写进 RAM 位图（任意块分配，FS 整体破坏）。合法 FS 的数据块与间接块
/// 都出自 `data_block_alloc`，必然满足本检查。
fn data_block_ok(c: &DevState, blk: u32) -> bool {
    if blk < c.data_start || blk >= c.total {
        return false;
    }
    ((blk - c.data_start) as usize) < MAX_BMAP_BLOCKS * BITS_PER_BLOCK
}

/// 读间接块第 `idx` 个指针（间接块号与返回的数据块号均做数据区校验）。
fn read_indirect(major: u32, block: usize, idx: usize) -> u32 {
    let mut buf = [0u8; BLK_SIZE];
    {
        let c = dev_mut(major);
        if !data_block_ok(c, block as u32) {
            return 0;
        }
    }
    if idx >= NINDIRECT || !meta_read(major, block, &mut buf) {
        return 0;
    }
    let v = get_u32(&buf, idx * 4);
    if v == 0 || !data_block_ok(dev_mut(major), v) {
        0
    } else {
        v
    }
}

/// 逻辑块号 → 磁盘块号；未分配或越界返回 0。
fn bmap(major: u32, inode: &[u8; INODE_SIZE], logical: usize) -> u32 {
    if logical < NDIRECT {
        let e = inode_direct(inode, logical);
        // 直接指针同样来自磁盘，必须过数据区校验（伪造指针 → 视为空洞）
        if e == 0 || !data_block_ok(dev_mut(major), e) {
            0
        } else {
            e
        }
    } else if logical < NDIRECT + NINDIRECT {
        let ind = inode_indirect(inode);
        if ind == 0 {
            0
        } else {
            read_indirect(major, ind as usize, logical - NDIRECT)
        }
    } else if logical < NDIRECT + NINDIRECT + NINDIRECT2 {
        let l = logical - NDIRECT - NINDIRECT;
        let db = inode_double(inode);
        if db == 0 {
            return 0;
        }
        let l1 = l / NINDIRECT;
        let l2 = l % NINDIRECT;
        let l1blk = read_indirect(major, db as usize, l1);
        if l1blk == 0 {
            0
        } else {
            read_indirect(major, l1blk as usize, l2)
        }
    } else if logical < MAX_BLOCKS {
        let l = logical - NDIRECT - NINDIRECT - NINDIRECT2;
        let tb = inode_triple(inode);
        if tb == 0 {
            return 0;
        }
        let l1 = l / NINDIRECT2;
        let l2 = (l / NINDIRECT) % NINDIRECT;
        let l3 = l % NINDIRECT;
        let l1blk = read_indirect(major, tb as usize, l1);
        if l1blk == 0 {
            return 0;
        }
        let l2blk = read_indirect(major, l1blk as usize, l2);
        if l2blk == 0 {
            0
        } else {
            read_indirect(major, l2blk as usize, l3)
        }
    } else {
        0
    }
}

/// 逻辑块号 → 磁盘块号，未分配则分配新数据块（含多级间接中间层）；失败 0。
fn bmap_alloc(major: u32, inode: &mut [u8; INODE_SIZE], logical: usize) -> u32 {
    if logical >= MAX_BLOCKS {
        return 0;
    }
    if logical < NDIRECT {
        let e = inode_direct(inode, logical);
        if e != 0 {
            return e;
        }
        // ★ ENOSPC 检测：data_block_alloc 失败返回 -1，`as u32` 后是 0xFFFFFFFF，
        // 此前 `blk == 0` 永远不成立 → 0xFFFFFFFF 毒指针被提交进 inode。
        let blk = data_block_alloc(major);
        if blk <= 0 {
            return 0;
        }
        let blk = blk as u32;
        inode_set_direct(inode, logical, blk);
        blk
    } else if logical < NDIRECT + NINDIRECT {
        let mut ind = inode_indirect(inode);
        if ind == 0 {
            let tmp = data_block_alloc(major);
            if tmp <= 0 {
                return 0;
            }
            ind = tmp as u32;
            inode_set_indirect(inode, ind);
        } else if !data_block_ok(dev_mut(major), ind) {
            // 伪造/损坏的间接指针：指向元数据区时 meta_read/meta_write 会
            // 改写 RAM 权威位图 —— 拒绝而非解引用。
            return 0;
        }
        let idx = logical - NDIRECT;
        let mut buf = [0u8; BLK_SIZE];
        if !meta_read(major, ind as usize, &mut buf) {
            return 0;
        }
        let e = get_u32(&buf, idx * 4);
        if e != 0 {
            return if data_block_ok(dev_mut(major), e) { e } else { 0 };
        }
        let blk = data_block_alloc(major);
        if blk <= 0 {
            return 0;
        }
        let blk = blk as u32;
        put_u32(&mut buf, idx * 4, blk);
        if !meta_write(major, ind as usize, &buf) {
            return 0;
        }
        blk
    } else if logical < NDIRECT + NINDIRECT + NINDIRECT2 {
        let l = logical - NDIRECT - NINDIRECT;
        let mut db = inode_double(inode);
        if db == 0 {
            let tmp = data_block_alloc(major);
            if tmp <= 0 {
                return 0;
            }
            db = tmp as u32;
            inode_set_double(inode, db);
        } else if !data_block_ok(dev_mut(major), db) {
            return 0;
        }
        let l1 = l / NINDIRECT;
        let l2 = l % NINDIRECT;
        let mut l1blk = read_indirect(major, db as usize, l1);
        if l1blk == 0 {
            let tmp = data_block_alloc(major);
            if tmp <= 0 {
                return 0;
            }
            l1blk = tmp as u32;
            let mut buf = [0u8; BLK_SIZE];
            if !meta_read(major, db as usize, &mut buf) {
                return 0;
            }
            put_u32(&mut buf, l1 * 4, l1blk);
            if !meta_write(major, db as usize, &buf) {
                return 0;
            }
        }
        let mut buf = [0u8; BLK_SIZE];
        if !meta_read(major, l1blk as usize, &mut buf) {
            return 0;
        }
        let e = get_u32(&buf, l2 * 4);
        if e != 0 {
            return if data_block_ok(dev_mut(major), e) { e } else { 0 };
        }
        let blk = data_block_alloc(major);
        if blk <= 0 {
            return 0;
        }
        let blk = blk as u32;
        put_u32(&mut buf, l2 * 4, blk);
        if !meta_write(major, l1blk as usize, &buf) {
            return 0;
        }
        blk
    } else {
        let l = logical - NDIRECT - NINDIRECT - NINDIRECT2;
        let mut tb = inode_triple(inode);
        if tb == 0 {
            let tmp = data_block_alloc(major);
            if tmp <= 0 {
                return 0;
            }
            tb = tmp as u32;
            inode_set_triple(inode, tb);
        } else if !data_block_ok(dev_mut(major), tb) {
            return 0;
        }
        let l1 = l / NINDIRECT2;
        let l2 = (l / NINDIRECT) % NINDIRECT;
        let l3 = l % NINDIRECT;
        let mut l1blk = read_indirect(major, tb as usize, l1);
        if l1blk == 0 {
            let tmp = data_block_alloc(major);
            if tmp <= 0 {
                return 0;
            }
            l1blk = tmp as u32;
            let mut buf = [0u8; BLK_SIZE];
            if !meta_read(major, tb as usize, &mut buf) {
                return 0;
            }
            put_u32(&mut buf, l1 * 4, l1blk);
            if !meta_write(major, tb as usize, &buf) {
                return 0;
            }
        }
        let mut l2blk = read_indirect(major, l1blk as usize, l2);
        if l2blk == 0 {
            let tmp = data_block_alloc(major);
            if tmp <= 0 {
                return 0;
            }
            l2blk = tmp as u32;
            let mut buf = [0u8; BLK_SIZE];
            if !meta_read(major, l1blk as usize, &mut buf) {
                return 0;
            }
            put_u32(&mut buf, l2 * 4, l2blk);
            if !meta_write(major, l1blk as usize, &buf) {
                return 0;
            }
        }
        let mut buf = [0u8; BLK_SIZE];
        if !meta_read(major, l2blk as usize, &mut buf) {
            return 0;
        }
        let e = get_u32(&buf, l3 * 4);
        if e != 0 {
            return if data_block_ok(dev_mut(major), e) { e } else { 0 };
        }
        let blk = data_block_alloc(major);
        if blk <= 0 {
            return 0;
        }
        let blk = blk as u32;
        put_u32(&mut buf, l3 * 4, blk);
        if !meta_write(major, l2blk as usize, &buf) {
            return 0;
        }
        blk
    }
}

// ==================== 目录项 ====================

fn put_name(buf: &mut [u8], off: usize, name: &str) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(MAX_NAME);
    buf[off..off + MAX_NAME + 1].fill(0);
    buf[off..off + n].copy_from_slice(&bytes[..n]);
}

fn name_eq(buf: &[u8], off: usize, name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() > MAX_NAME {
        return false;
    }
    for (i, &c) in bytes.iter().enumerate() {
        if buf[off + i] != c {
            return false;
        }
    }
    buf[off + bytes.len()] == 0
}

fn get_name(buf: &[u8], off: usize) -> [u8; MAX_NAME + 1] {
    let mut out = [0u8; MAX_NAME + 1];
    for (i, o) in out.iter_mut().enumerate() {
        let c = buf[off + i];
        if c == 0 {
            break;
        }
        *o = c;
    }
    out
}

/// 在目录 `dir_ino` 中追加一条 (entry_ino, name) 目录项，返回 0；失败 -1。
fn dir_add_entry(major: u32, dir_ino: u32, entry_ino: u32, name: &str) -> i32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, dir_ino, &mut inode) {
        return -1;
    }
    if inode_type(&inode) != T_DIR {
        return -1;
    }
    let size = inode_size(&inode) as usize;

    // 1) 在已分配块中寻找空目录项（ino == 0）
    for i in 0..NDIRECT {
        let blk = inode_direct(&inode, i);
        // 伪造/损坏指针（指向位图/inode 表等元数据区）按空洞处理，与 bmap
        // 同策略：目录数据写进 RAM 权威位图 = 任意块分配位被翻转
        if blk == 0 || !data_block_ok(dev_mut(major), blk) {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !meta_read(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            if get_u32(&buf, e) == 0 {
                put_u32(&mut buf, e, entry_ino);
                put_name(&mut buf, e + 4, name);
                // ★ 元数据写失败必须上抛：目录项未暂存进事务，提交后磁盘目录
                // 仍缺此项，调用方整体回滚否则会出现「目录项丢失」的不一致。
                if !meta_write(major, blk as usize, &buf) {
                    return -1;
                }
                return 0;
            }
        }
    }

    // 2) 分配新数据块，写入首条目录项；目录已写满则失败
    let logical = size / BLK_SIZE;
    if logical >= NDIRECT || inode_direct(&inode, logical) != 0 {
        return -1;
    }
    let blk = bmap_alloc(major, &mut inode, logical);
    if blk == 0 {
        return -1;
    }
    let mut buf = [0u8; BLK_SIZE];
    put_u32(&mut buf, 0, entry_ino);
    put_name(&mut buf, 4, name);
    if !meta_write(major, blk as usize, &buf) {
        return -1;
    }
    inode_set_size(&mut inode, (logical as u64 + 1) * BLK_SIZE as u64);
    if !write_inode(major, dir_ino, &inode) {
        return -1;
    }
    0
}

/// 内部：从目录 `dir_ino` 中删除名字对应的目录项（不检查目标类型），
/// 返回被删除的 inode 号；未找到返回 -1。
fn dir_remove_entry(major: u32, dir_ino: u32, name: &str) -> i32 {
    let mut dinode = [0u8; INODE_SIZE];
    if !read_inode(major, dir_ino, &mut dinode) {
        return -1;
    }
    if inode_type(&dinode) != T_DIR {
        return -1;
    }
    for i in 0..NDIRECT {
        let blk = inode_direct(&dinode, i);
        // 伪造/损坏指针按空洞处理（同 bmap）：防止目录数据写进元数据区
        if blk == 0 || !data_block_ok(dev_mut(major), blk) {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !meta_read(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            let ino = get_u32(&buf, e);
            if ino != 0 && name_eq(&buf, e + 4, name) {
                put_u32(&mut buf, e, 0);
                // ★ 元数据写失败必须上抛：目录项删除未暂存，提交后旧目录项仍
                // 留在磁盘，调用方按失败回滚（否则旧项与新项并存或重复 ino）。
                if !meta_write(major, blk as usize, &buf) {
                    return -1;
                }
                return ino as i32;
            }
        }
    }
    -1
}

// ==================== 对外接口 ====================

/// 格式化 `major` 设备为 SFS v2，返回 0；失败 -1。
pub fn mkfs(major: u32) -> i32 {
    fs_lock();
    let r = mkfs_unlocked(major);
    fs_unlock();
    r
}

fn mkfs_unlocked(major: u32) -> i32 {
    let dev = match crate::blkdev::get_blkdev(major) {
        Some(d) => d,
        None => return -1,
    };
    let total = dev.ops.num_sectors / crate::blkdev::SECTORS_PER_BLOCK;
    let (bmap_blocks, inode_start, jlog_start, jlog_blocks, data_start) = layout(total);
    // ★ 布局哨兵：位图块数超上限（卷 > MAX_BMAP_BLOCKS×32768 块）直接拒绝，
    // 防止后续 `c.bmap[b]`（b < bmap_blocks）越界写穿 .bss 相邻全局。
    if bmap_blocks == usize::MAX || bmap_blocks > MAX_BMAP_BLOCKS {
        crate::uart_send_str("[sfs] mkfs: 卷过大 (位图溢出)\n");
        return -1;
    }
    if data_start >= total {
        return -1;
    }

    // 复位全局事务状态（mkfs 可在任意时刻调用）
    unsafe {
        TXN_DEPTH = 0;
        TXN_MAJOR = u32::MAX;
        PENDING_N = 0;
    }

    let c = dev_mut(major);
    *c = EMPTY_DEV;
    c.used = true;
    c.major = major;
    c.total = total as u32;
    c.inode_blocks = INODE_BLOCKS as u32;
    c.bmap_start = BMAP_START as u32;
    c.bmap_blocks = bmap_blocks as u32;
    c.inode_start = inode_start as u32;
    c.jlog_start = jlog_start as u32;
    c.jlog_blocks = jlog_blocks as u32;
    c.data_start = data_start as u32;
    c.num_inodes = MAX_INODES as u32;
    c.root_ino = 1;
    c.free_inodes = (MAX_INODES - 1) as u32;
    c.free_blocks = (total - data_start) as u32;
    c.ino_hint = 1;
    c.blk_hint = 0;
    c.jlog_head_seq = 0;
    c.jlog_head_abs = 0;
    c.jlog_next_abs = 0;
    c.next_seq = 1;

    // 清零元数据区（inode 表不清零会残留垃圾 inode）
    let zero = [0u8; BLK_SIZE];
    write_block(major, SB0, &zero);
    write_block(major, SB1, &zero);
    c.imap.fill(0);
    bit_set(&mut c.imap, 0); // 保留 inode 0
    for b in 0..bmap_blocks {
        c.bmap[b].fill(0);
        write_block(major, BMAP_START + b, &zero);
    }
    for b in 0..INODE_BLOCKS {
        write_block(major, inode_start + b, &zero);
    }
    for b in 0..jlog_blocks {
        write_block(major, jlog_start + b, &zero);
    }
    write_superblock(major);

    // 通过日志事务创建根 inode（"." / ".."）
    txn_begin(major);
    let root_ino = inode_alloc(major);
    if root_ino != 1 {
        txn_abort(major);
        return -1;
    }
    let mut root = [0u8; INODE_SIZE];
    inode_set_type(&mut root, T_DIR);
    inode_set_nlink(&mut root, 1);
    inode_set_gen(&mut root, 1);
    write_inode(major, root_ino as u32, &root);
    if dir_add_entry(major, root_ino as u32, root_ino as u32, ".") != 0
        || dir_add_entry(major, root_ino as u32, root_ino as u32, "..") != 0
    {
        txn_abort(major);
        return -1;
    }
    txn_commit(major);
    checkpoint(major);
    0
}

/// 检查 `major` 设备是否带 SFS v2 超级块魔数（不执行挂载）。
pub fn fs_probe(major: u32) -> bool {
    fs_lock();
    let r = fs_probe_unlocked(major);
    fs_unlock();
    r
}

fn fs_probe_unlocked(major: u32) -> bool {
    let mut sb = [0u8; BLK_SIZE];
    (read_block(major, SB0, &mut sb) && get_u32(&sb, SB_MAGIC) == MAGIC)
        || (read_block(major, SB1, &mut sb) && get_u32(&sb, SB_MAGIC) == MAGIC)
}

/// 挂载 `major` 设备：读超级块 → 加载位图 → 崩溃恢复 → 重置日志。
/// 返回 0；失败 -1。
pub fn mount(major: u32) -> i32 {
    fs_lock();
    let r = mount_unlocked(major);
    fs_unlock();
    r
}

fn mount_unlocked(major: u32) -> i32 {
    unsafe {
        TXN_DEPTH = 0;
        TXN_MAJOR = u32::MAX;
        PENDING_N = 0;
    }
    let mut sb = [0u8; BLK_SIZE];
    if !read_sb(major, &mut sb) {
        return -1;
    }
    let c = dev_mut(major);
    *c = EMPTY_DEV;
    c.used = true;
    c.major = major;
    if !parse_sb(&sb, c) {
        c.used = false;
        return -1;
    }

    // 加载位图到 RAM（检查点状态）
    if !read_block(major, IMAP_BLOCK, &mut c.imap) {
        return -1;
    }
    for b in 0..c.bmap_blocks as usize {
        if !read_block(major, BMAP_START + b, &mut c.bmap[b]) {
            return -1;
        }
    }

    // 崩溃恢复：重放已提交日志（写回磁盘位图/元数据）
    let n = journal_recover(major);
    if n < 0 {
        return -1; // 重放未完整完成（掉电/IO 错误）：不能在此状态上继续运行
    }
    if n > 0 {
        crate::uart_send_str("[sfs] 日志恢复: 已重放 ");
        crate::print_dec(n as u64);
        crate::uart_send_str(" 条记录\n");
    }

    // 恢复可能改写了位图：必须重新加载 RAM 缓存，否则 RAM 位图是
    // 「检查点 + 未重放」的陈旧状态，与磁盘/元数据不一致
    if !read_block(major, IMAP_BLOCK, &mut c.imap) {
        return -1;
    }
    for b in 0..c.bmap_blocks as usize {
        if !read_block(major, BMAP_START + b, &mut c.bmap[b]) {
            return -1;
        }
    }

    // 重新统计空闲计数（位图权威）
    c.free_inodes = 0;
    for ino in 1..MAX_INODES {
        if !bit_get(&c.imap, ino) {
            c.free_inodes += 1;
        }
    }
    c.free_blocks = 0;
    // 位图数组上限钳制：坏超级块给出超大 ndata 时只统计位图可表达的块数。
    // saturating_sub：data_start > total（坏卷几何）时按 0 处理，防回绕。
    let ndata = c
        .total
        .saturating_sub(c.data_start)
        .min((MAX_BMAP_BLOCKS * BITS_PER_BLOCK) as u32) as usize;
    for idx in 0..ndata {
        if !bmap_get(c, idx) {
            c.free_blocks += 1;
        }
    }

    // 持久化复位后的日志游标
    write_superblock(major);
    0
}

/// 根 inode 号（从超级块读取）。
pub fn fs_root_ino(major: u32) -> u32 {
    fs_lock();
    let r = fs_root_ino_unlocked(major);
    fs_unlock();
    r
}

fn fs_root_ino_unlocked(major: u32) -> u32 {
    let mut sb = [0u8; BLK_SIZE];
    if !read_sb(major, &mut sb) {
        return 0;
    }
    get_u32(&sb, SB_ROOT_INO)
}

/// 在目录 `dir_ino` 中查找 `name`，返回目标 inode 号；未找到 -1。
pub fn fs_lookup(major: u32, dir_ino: u32, name: &str) -> i32 {
    fs_lock();
    let r = fs_lookup_unlocked(major, dir_ino, name);
    fs_unlock();
    r
}

fn fs_lookup_unlocked(major: u32, dir_ino: u32, name: &str) -> i32 {
    if name.is_empty() || name.len() > MAX_NAME {
        return -1;
    }
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, dir_ino, &mut inode) {
        return -1;
    }
    if inode_type(&inode) != T_DIR {
        return -1;
    }
    for i in 0..NDIRECT {
        let blk = inode_direct(&inode, i);
        // 伪造/损坏指针（指向位图/inode 表等元数据区）按空洞处理，与 bmap
        // 同策略：目录数据写进 RAM 权威位图 = 任意块分配位被翻转
        if blk == 0 || !data_block_ok(dev_mut(major), blk) {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !meta_read(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            let ino = get_u32(&buf, e);
            if ino != 0 && name_eq(&buf, e + 4, name) {
                return ino as i32;
            }
        }
    }
    -1
}

/// 在目录 `dir_ino` 中创建类型为 `typ` 的新 inode（名为 `name`），返回 inode 号；失败 -1。
pub fn fs_create(major: u32, dir_ino: u32, name: &str, typ: u16, rdev: u32) -> i32 {
    fs_lock();
    let r = fs_create_unlocked(major, dir_ino, name, typ, rdev);
    fs_unlock();
    r
}

fn fs_create_unlocked(major: u32, dir_ino: u32, name: &str, typ: u16, rdev: u32) -> i32 {
    if name.is_empty() || name.len() > MAX_NAME {
        return -1;
    }
txn_begin(major);
    if fs_lookup_unlocked(major, dir_ino, name) >= 0 {
        txn_abort(major);
        return -1; // 已存在
    }
    let ino = inode_alloc(major);
    if ino < 0 {
        txn_abort(major);
        return -1;
    }
    let mut inode = [0u8; INODE_SIZE];
    inode_set_type(&mut inode, typ);
    inode_set_nlink(&mut inode, 1);
    inode_set_rdev(&mut inode, rdev);
    inode_set_gen(&mut inode, 1);
    // 由非 root 身份创建：在同一事务内记录归属（默认 0644 / 目录 0755），
    // 避免“先创建、后补写 owner”之间掉电造成无归属窗口。
    let cuid = crate::process::current_uid();
    if cuid != 0 {
        put_u32(&mut inode, I_UID, cuid);
        put_u32(&mut inode, I_GID, crate::process::current_gid());
        put_u16(&mut inode, I_MODE, if typ == T_DIR { 0o755 } else { 0o644 });
        put_u16(&mut inode, I_OWNF, OWNF_VALID);
    }
    if !write_inode(major, ino as u32, &inode) {
        txn_abort(major);
        return -1;
    }
    if dir_add_entry(major, dir_ino, ino as u32, name) != 0 {
        txn_abort(major);
        return -1;
    }
    txn_commit(major);
    ino
}

/// 读取 inode 的归属元数据 (uid, gid, mode)；无归属（旧文件 / root 创建）返回 None。
pub fn fs_owner(major: u32, ino: u32) -> Option<(u32, u32, u16)> {
    fs_lock();
    let mut inode = [0u8; INODE_SIZE];
    let r = if read_inode(major, ino, &mut inode) && get_u16(&inode, I_OWNF) & OWNF_VALID != 0 {
        Some((get_u32(&inode, I_UID), get_u32(&inode, I_GID), get_u16(&inode, I_MODE)))
    } else {
        None
    };
    fs_unlock();
    r
}

/// 修改归属元数据（chmod / chown 的底层）；`None` 的字段保持不变。
/// inode 原本无归属时以 root:root 与按类型的默认权限起步，再套用本次修改。
/// 一个日志事务内完成；失败返回 -1。
pub fn fs_set_owner(major: u32, ino: u32, uid: Option<u32>, gid: Option<u32>, mode: Option<u16>) -> i32 {
    fs_lock();
    txn_begin(major);
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        txn_abort(major);
        fs_unlock();
        return -1;
    }
    if get_u16(&inode, I_OWNF) & OWNF_VALID == 0 {
        put_u32(&mut inode, I_UID, 0);
        put_u32(&mut inode, I_GID, 0);
        let default_mode = if inode_type(&inode) == T_DIR { 0o755 } else { 0o644 };
        put_u16(&mut inode, I_MODE, default_mode);
    }
    if let Some(u) = uid {
        put_u32(&mut inode, I_UID, u);
    }
    if let Some(g) = gid {
        put_u32(&mut inode, I_GID, g);
    }
    if let Some(m) = mode {
        put_u16(&mut inode, I_MODE, m & 0o7777);
    }
    put_u16(&mut inode, I_OWNF, OWNF_VALID);
    if !write_inode(major, ino, &inode) {
        txn_abort(major);
        fs_unlock();
        return -1;
    }
    txn_commit(major);
    fs_unlock();
    0
}

/// 在目录 `dir_ino` 下创建名为 `name` 的符号链接，目标为 `target`，返回 inode 号；失败 -1。
///
/// 目标路径作为 T_SYM inode 的文件数据存储。整操作一个事务：任一步失败整体回滚。
pub fn fs_symlink(major: u32, dir_ino: u32, name: &str, target: &str) -> i32 {
    fs_lock();
    let r = fs_symlink_unlocked(major, dir_ino, name, target);
    fs_unlock();
    r
}

fn fs_symlink_unlocked(major: u32, dir_ino: u32, name: &str, target: &str) -> i32 {
    if name.is_empty() || name.len() > MAX_NAME || target.len() > 240 {
        return -1;
    }
    txn_begin(major);
    let ino = fs_create_unlocked(major, dir_ino, name, T_SYM, 0);
    if ino < 0 {
        txn_abort(major);
        return ino;
    }
    if fs_write_unlocked(major, ino as u32, 0, target.as_bytes()) != target.len() as i32 {
        txn_abort(major);
        return -1;
    }
    txn_commit(major);
    ino
}

/// 读取符号链接 `ino` 的目标字符串（不跟随），写入 `buf`，返回字节数；失败 -1。
pub fn fs_readlink(major: u32, ino: u32, buf: &mut [u8]) -> i32 {
    fs_lock();
    let r = fs_readlink_unlocked(major, ino, buf);
    fs_unlock();
    r
}

fn fs_readlink_unlocked(major: u32, ino: u32, buf: &mut [u8]) -> i32 {
    let size = fs_size_unlocked(major, ino) as usize;
    if size == 0 {
        return 0;
    }
    let n = size.min(buf.len());
    fs_read_unlocked(major, ino, 0, &mut buf[..n])
}

/// 创建目录，返回 inode 号；失败 -1。
pub fn fs_mkdir(major: u32, dir_ino: u32, name: &str) -> i32 {
    fs_lock();
    let r = fs_mkdir_unlocked(major, dir_ino, name);
    fs_unlock();
    r
}

fn fs_mkdir_unlocked(major: u32, dir_ino: u32, name: &str) -> i32 {
    if name.is_empty() || name.len() > MAX_NAME {
        return -1;
    }
    txn_begin(major);
    let ino = fs_create_unlocked(major, dir_ino, name, T_DIR, 0);
    if ino < 0 {
        txn_abort(major);
        return ino;
    }
    if dir_add_entry(major, ino as u32, ino as u32, ".") != 0
        || dir_add_entry(major, ino as u32, dir_ino, "..") != 0
    {
        txn_abort(major);
        return -1;
    }
    txn_commit(major);
    ino
}

/// 从 inode `ino` 的 `offset` 处读取，返回实际读到的字节数；失败 -1。
pub fn fs_read(major: u32, ino: u32, offset: usize, buf: &mut [u8]) -> i32 {
    fs_lock();
    let r = fs_read_unlocked(major, ino, offset, buf);
    fs_unlock();
    r
}

fn fs_read_unlocked(major: u32, ino: u32, offset: usize, buf: &mut [u8]) -> i32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return -1;
    }
    let size = inode_size(&inode) as usize;
    if offset >= size {
        return 0;
    }
    let mut total = 0usize;
    let mut pos = offset;
    while total < buf.len() && pos < size {
        let logical = pos / BLK_SIZE;
        let blk = bmap(major, &inode, logical);
        let n = (buf.len() - total).min(BLK_SIZE - pos % BLK_SIZE).min(size - pos);
        if blk == 0 {
            // 空洞：按零填充
            buf[total..total + n].fill(0);
        } else {
            let mut tmp = [0u8; BLK_SIZE];
            if !read_block(major, blk as usize, &mut tmp) {
                break;
            }
            let off = pos % BLK_SIZE;
            buf[total..total + n].copy_from_slice(&tmp[off..off + n]);
        }
        total += n;
        pos += n;
    }
    total as i32
}

/// 从 inode `ino` 的 `offset` 处写入，返回实际写入字节数；失败 -1。
///
/// 数据块内容直接写盘（不经日志）；元数据（位图/inode/间接块）经日志事务
/// 提交，保证掉电后数据块要么已写入、要么其元数据引用尚未提交。
pub fn fs_write(major: u32, ino: u32, offset: usize, buf: &[u8]) -> i32 {
    fs_lock();
    let r = fs_write_unlocked(major, ino, offset, buf);
    fs_unlock();
    r
}

/// O_APPEND 原子追加：取尾与写入在同一临界区完成。
///
/// 若分两步（先 fs_size 后 fs_write，各自加锁），两个并发追加者会取到
/// 同一尾部偏移、写至同一位置——后写覆盖先写（文件信箱丢消息）。文件
/// 信箱（audiod.ctl / rild.cmd / wifid.pipe 等多写者管道）依赖本语义。
pub fn fs_write_append(major: u32, ino: u32, buf: &[u8]) -> i32 {
    fs_lock();
    let off = fs_size_unlocked(major, ino) as usize;
    let r = fs_write_unlocked(major, ino, off, buf);
    fs_unlock();
    r
}

fn fs_write_unlocked(major: u32, ino: u32, offset: usize, buf: &[u8]) -> i32 {
    txn_begin(major);
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        txn_abort(major);
        return -1;
    }
    if inode_type(&inode) == T_DIR {
        txn_abort(major);
        return -1;
    }
    let mut total = 0usize;
    let mut pos = offset;
    let mut limit_hit = false;
    while total < buf.len() {
        let logical = pos / BLK_SIZE;
        if logical >= MAX_BLOCKS {
            limit_hit = true;
            break;
        }
        let blk = bmap_alloc(major, &mut inode, logical);
        if blk == 0 {
            break;
        }
        let off = pos % BLK_SIZE;
        let n = (buf.len() - total).min(BLK_SIZE - off);
        if off == 0 && n == BLK_SIZE {
            let mut tmp = [0u8; BLK_SIZE];
            tmp.copy_from_slice(&buf[total..total + BLK_SIZE]);
            if !write_block(major, blk as usize, &tmp) {
                break;
            }
            wear_add(blk as usize);
        } else {
            let mut tmp = [0u8; BLK_SIZE];
            // 读-改-写：读失败必须中止，否则 tmp 未初始化会清掉整块数据
            if !read_block(major, blk as usize, &mut tmp) {
                break;
            }
            tmp[off..off + n].copy_from_slice(&buf[total..total + n]);
            if !write_block(major, blk as usize, &tmp) {
                break;
            }
            wear_add(blk as usize);
        }
        total += n;
        pos += n;
        if pos > inode_size(&inode) as usize {
            inode_set_size(&mut inode, pos as u64);
        }
    }
    // 部分成功也提交（记录已分配块与最新大小），避免块分配成功但 inode
    // 未持久化导致块泄漏
    // ★ inode 元数据写失败：暂存未就绪则提交后磁盘 inode 不更新（块泄漏/
    // 大小错乱），按未提交回滚本次事务。
    if !write_inode(major, ino, &inode) {
        txn_abort(major);
        return -1;
    }
    txn_commit(major);
    if total == 0 && limit_hit {
        return -1;
    }
    total as i32
}

/// 释放单间接块链：其指向的全部数据块 + 间接块本身。
fn free_indirect_chain(major: u32, block: usize) {
    // 顶层块号同样来自磁盘 inode（可被伪造）：把元数据区（位图等）当间接
    // 块解析会按其内容误释放真实数据块。与 read_indirect 同款校验。
    {
        let c = dev_mut(major);
        if !data_block_ok(c, block as u32) {
            return;
        }
    }
    let mut buf = [0u8; BLK_SIZE];
    if meta_read(major, block, &mut buf) {
        for i in 0..NINDIRECT {
            let b = get_u32(&buf, i * 4);
            if b != 0 {
                data_block_free(major, b);
            }
        }
    }
    data_block_free(major, block as u32);
}

/// 把 inode `ino` 截断为 0（释放全部数据块与间接块），返回 0；失败 -1。
///
/// 整个截断是一个日志事务：位图释放与 inode 清空原子提交，掉电不会出现
/// 「块已释放但 inode 仍引用」的撕裂状态。
pub fn fs_truncate(major: u32, ino: u32) -> i32 {
    fs_lock();
    let r = fs_truncate_unlocked(major, ino);
    fs_unlock();
    r
}

fn fs_truncate_unlocked(major: u32, ino: u32) -> i32 {
    txn_begin(major);
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        txn_abort(major);
        return -1;
    }
    for i in 0..NDIRECT {
        let blk = inode_direct(&inode, i);
        if blk != 0 {
            data_block_free(major, blk);
            inode_set_direct(&mut inode, i, 0);
        }
    }
    let ind = inode_indirect(&inode);
    if ind != 0 {
        free_indirect_chain(major, ind as usize);
        inode_set_indirect(&mut inode, 0);
    }
    let db = inode_double(&inode);
    if db != 0 {
        for l1 in 0..NINDIRECT {
            let l1blk = read_indirect(major, db as usize, l1);
            if l1blk != 0 {
                free_indirect_chain(major, l1blk as usize);
            }
        }
        data_block_free(major, db);
        inode_set_double(&mut inode, 0);
    }
    let tb = inode_triple(&inode);
    if tb != 0 {
        for l1 in 0..NINDIRECT {
            let l1blk = read_indirect(major, tb as usize, l1);
            if l1blk != 0 {
                for l2 in 0..NINDIRECT {
                    let l2blk = read_indirect(major, l1blk as usize, l2);
                    if l2blk != 0 {
                        free_indirect_chain(major, l2blk as usize);
                    }
                }
                data_block_free(major, l1blk);
            }
        }
        data_block_free(major, tb);
        inode_set_triple(&mut inode, 0);
    }
    inode_set_size(&mut inode, 0);
    // inode 写回失败时中止事务：位图里已staged的块释放随事务一起丢弃，
    // 否则 checkpoint 后位图标记空闲而旧 inode 仍指向这些块 → 被新文件
    // 复用后跨文件数据踩踏。
    if !write_inode(major, ino, &inode) {
        txn_abort(major);
        return -1;
    }
    txn_commit(major);
    0
}

/// 从目录 `dir_ino` 中删除 `name`，并释放其 inode 与数据块，返回 0；失败 -1。
///
/// 摘目录项 + 截断 + 释放 inode 在同一个日志事务中原子完成。
pub fn fs_unlink(major: u32, dir_ino: u32, name: &str) -> i32 {
    fs_lock();
    let r = fs_unlink_unlocked(major, dir_ino, name);
    fs_unlock();
    r
}

fn fs_unlink_unlocked(major: u32, dir_ino: u32, name: &str) -> i32 {
    if name.is_empty() || name.len() > MAX_NAME || name == "." || name == ".." {
        return -1;
    }
    txn_begin(major);
    let ino = dir_remove_entry(major, dir_ino, name);
    if ino < 0 {
        txn_abort(major);
        return -1;
    }
    let ino = ino as u32;
    if fs_type_unlocked(major, ino) == T_DIR {
        txn_abort(major);
        return -1; // 拒绝删除目录
    }
    if fs_truncate_unlocked(major, ino) != 0 {
        // truncate 失败（数据块释放不完整）：提交会把 inode 清零而数据块
        // 仍在位图标记已用 → 孤儿块永久泄漏（sfs_check 可检出但无人回收）。
        // 回滚整个事务：目录项未删、inode 完好，unlink 报错由调用方重试。
        txn_abort(major);
        return -1;
    }
    inode_free(major, ino);
    txn_commit(major);
    0
}

/// 重命名：把 `old_dir` 下的 `old_name` 移到 `new_dir` 下的 `new_name`。
///
/// 仅支持普通文件与符号链接；目标已存在且同为普通文件/符号链接时按 POSIX
/// 语义原子替换（同一日志事务内摘除目标目录项并截断、回收其 inode——掉电
/// 不会出现「旧项已删新项未加」或「目标半删」状态）。目标为目录、源不存在
/// 或回收失败则整体回滚。成功返回 0；失败 -1。
pub fn fs_rename(major: u32, old_dir: u32, old_name: &str, new_dir: u32, new_name: &str) -> i32 {
    fs_lock();
    let r = fs_rename_unlocked(major, old_dir, old_name, new_dir, new_name);
    fs_unlock();
    r
}

fn fs_rename_unlocked(major: u32, old_dir: u32, old_name: &str, new_dir: u32, new_name: &str) -> i32 {
    if old_name.is_empty()
        || old_name.len() > MAX_NAME
        || new_name.is_empty()
        || new_name.len() > MAX_NAME
        || old_name == "."
        || old_name == ".."
        || new_name == "."
        || new_name == ".."
    {
        return -1;
    }
    if old_dir == new_dir && old_name == new_name {
        return 0;
    }
    txn_begin(major);
    let ino = fs_lookup_unlocked(major, old_dir, old_name);
    if ino < 0 {
        txn_abort(major);
        return -1;
    }
    let ityp = fs_type_unlocked(major, ino as u32);
    if ityp != T_REG && ityp != T_SYM {
        txn_abort(major);
        return -1;
    }
    // POSIX 覆盖语义：目标已存在时在同一事务内摘除并回收（摘目录项 + 截断
    // + inode_free，与 fs_unlink_unlocked 同序列）。目录项替换与源移动对
    // 掉电是单个原子事务。「临时文件 + rename 原子提交」依赖此语义：崩溃
    // 后要么看到完整旧文件、要么看到完整新文件，绝无中间态。
    let tgt = fs_lookup_unlocked(major, new_dir, new_name);
    if tgt >= 0 {
        let tgt = tgt as u32;
        let tty = fs_type_unlocked(major, tgt);
        if tgt != ino as u32 && (tty != T_REG && tty != T_SYM) {
            // 目标是目录/FIFO 等不可覆盖类型
            txn_abort(major);
            return -1;
        }
        if tgt != ino as u32 {
            if dir_remove_entry(major, new_dir, new_name) < 0 {
                txn_abort(major);
                return -1;
            }
            if fs_truncate_unlocked(major, tgt) != 0 {
                txn_abort(major);
                return -1;
            }
            inode_free(major, tgt);
        }
    }
    if dir_remove_entry(major, old_dir, old_name) < 0 {
        txn_abort(major);
        return -1;
    }
    if dir_add_entry(major, new_dir, ino as u32, new_name) != 0 {
        txn_abort(major);
        return -1;
    }
    txn_commit(major);
    0
}

/// 读取目录第 `index` 个条目，返回其 inode 号并把名字写入 `name`；结束返回 -1。
pub fn fs_readdir(major: u32, ino: u32, index: usize, name: &mut [u8]) -> i32 {
    fs_lock();
    let r = fs_readdir_unlocked(major, ino, index, name);
    fs_unlock();
    r
}

fn fs_readdir_unlocked(major: u32, ino: u32, index: usize, name: &mut [u8]) -> i32 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return -1;
    }
    if inode_type(&inode) != T_DIR {
        return -1;
    }
    let mut seen = 0usize;
    for i in 0..NDIRECT {
        let blk = inode_direct(&inode, i);
        // 伪造/损坏指针（指向位图/inode 表等元数据区）按空洞处理，与 bmap
        // 同策略：目录数据写进 RAM 权威位图 = 任意块分配位被翻转
        if blk == 0 || !data_block_ok(dev_mut(major), blk) {
            break;
        }
        let mut buf = [0u8; BLK_SIZE];
        if !meta_read(major, blk as usize, &mut buf) {
            return -1;
        }
        for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
            let eino = get_u32(&buf, e);
            if eino == 0 {
                continue;
            }
            if seen == index {
                let nm = get_name(&buf, e + 4);
                let len = nm.iter().position(|&c| c == 0).unwrap_or(nm.len());
                let n = len.min(name.len());
                name[..n].copy_from_slice(&nm[..n]);
                if n < name.len() {
                    name[n] = 0;
                }
                return eino as i32;
            }
            seen += 1;
        }
    }
    -1
}

/// 空闲数据块数（RAM 位图权威计数）。
pub fn fs_free_blocks(major: u32) -> u32 {
    fs_lock();
    let r = dev_mut(major).free_blocks;
    fs_unlock();
    r
}

/// 数据块总数。
pub fn fs_total_blocks(major: u32) -> u32 {
    fs_lock();
    let c = dev_mut(major);
    let r = c.total.saturating_sub(c.data_start);
    fs_unlock();
    r
}

/// 块大小（字节）：SFS 以 4KB 块为计数单位。
pub fn fs_block_size(_major: u32) -> u32 {
    BLK_SIZE as u32
}

/// 空闲 inode 数（RAM 位图权威计数；inode 耗尽的排查出口——df 直接可见）。
pub fn fs_free_inodes(major: u32) -> u32 {
    fs_lock();
    let r = dev_mut(major).free_inodes;
    fs_unlock();
    r
}

/// inode 总数（卷容量）。
pub fn fs_total_inodes(major: u32) -> u32 {
    fs_lock();
    let r = dev_mut(major).num_inodes;
    fs_unlock();
    r
}

/// 读取 inode 的类型码。
pub fn fs_type(major: u32, ino: u32) -> u16 {
    fs_lock();
    let r = fs_type_unlocked(major, ino);
    fs_unlock();
    r
}

fn fs_type_unlocked(major: u32, ino: u32) -> u16 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return 0;
    }
    inode_type(&inode)
}

/// 读取 inode 的大小（字节，64 位）。
pub fn fs_size(major: u32, ino: u32) -> u64 {
    fs_lock();
    let r = fs_size_unlocked(major, ino);
    fs_unlock();
    r
}

fn fs_size_unlocked(major: u32, ino: u32) -> u64 {
    let mut inode = [0u8; INODE_SIZE];
    if !read_inode(major, ino, &mut inode) {
        return 0;
    }
    inode_size(&inode)
}

/// 读取 inode 的设备号（CHR/BLK 用）。
pub fn fs_rdev(major: u32, ino: u32) -> u32 {
    fs_lock();
    let mut inode = [0u8; INODE_SIZE];
    let r = if read_inode(major, ino, &mut inode) {
        inode_rdev(&inode)
    } else {
        0
    };
    fs_unlock();
    r
}

/// fsync：把当前设备的所有未落盘元数据（RAM 位图 + 超级块）刷盘并 FLUSH。
/// 若有未完成事务先提交。返回 0；失败 -1。
pub fn fs_checkpoint(major: u32) -> i32 {
    fs_lock();
    unsafe {
        if TXN_DEPTH > 0 {
            if TXN_MAJOR == major {
                let _ = commit_inner(major);
            } else {
                let _ = commit_inner(TXN_MAJOR);
            }
        }
    }
    let r = checkpoint(major);
    fs_unlock();
    r
}

/// 磨损统计：返回各分区写计数（供测试/诊断）。
pub fn fs_wear_report() -> (u64, u32, u32) {
    fs_lock();
    let r = unsafe {
        let mut total = 0u64;
        let mut min = u32::MAX;
        let mut max = 0u32;
        for &w in WEAR.iter() {
            total += w as u64;
            if w < min {
                min = w;
            }
            if w > max {
                max = w;
            }
        }
        (total, min, max)
    };
    fs_unlock();
    r
}

/// fsck 语义一致性检查：超级块、位图 ↔ inode 表 ↔ 块列表 ↔ 目录项。
/// 返回 0 = 一致；-1 = 发现错误（打印明细）。
pub fn sfs_check(major: u32) -> i32 {
    fs_lock();
    let r = sfs_check_unlocked(major);
    fs_unlock();
    r
}

fn sfs_check_unlocked(major: u32) -> i32 {
    let mut errs = 0usize;
    let mut sb = [0u8; BLK_SIZE];
    if !read_sb(major, &mut sb) {
        crate::uart_send_str("[sfs-check] 超级块无效\n");
        return -1;
    }
    let c = dev_mut(major);
    let ndata = c.total.saturating_sub(c.data_start) as usize;
    // 引用标记位图（内存）：MAX_BMAP_BLOCKS=256 时为 1MB，不能放任务栈上
    // （256KB 栈必然溢出写穿栈底）。移到静态全局（.bss），fs_lock 串行化保证
    // 单调用者；每次进入先清零（复用不残留上次检查的标记）。
    static mut CHECK_REFS: [[u8; BLK_SIZE]; MAX_BMAP_BLOCKS] = [[0; BLK_SIZE]; MAX_BMAP_BLOCKS];
    let refs = unsafe { &mut CHECK_REFS };
    for b in refs.iter_mut() {
        b.fill(0);
    }
    // 校验并标记一个被引用块（宏内联访问局部变量；ino 作为参数传入）
    macro_rules! check_blk {
        ($ino:expr, $blk:expr) => {{
            let blk = $blk;
            if blk != 0 {
                if blk < c.data_start || blk as usize >= c.total as usize {
                    crate::uart_send_str("[sfs-check] inode ");
                    crate::print_dec($ino as u64);
                    crate::uart_send_str(" 引用越界块 ");
                    crate::print_dec(blk as u64);
                    crate::uart_send_str("\n");
                    errs += 1;
                } else {
                    let idx = (blk - c.data_start) as usize;
                    if idx >= ndata {
                        crate::uart_send_str("[sfs-check] inode ");
                        crate::print_dec($ino as u64);
                        crate::uart_send_str(" 引用超出位图范围的块: ");
                        crate::print_dec(blk as u64);
                        crate::uart_send_str("\n");
                        errs += 1;
                    } else if !bmap_get(c, idx) {
                        crate::uart_send_str("[sfs-check] inode ");
                        crate::print_dec($ino as u64);
                        crate::uart_send_str(" 引用未分配的块 ");
                        crate::print_dec(blk as u64);
                        crate::uart_send_str("\n");
                        errs += 1;
                    } else {
                        bit_set(&mut refs[idx / BITS_PER_BLOCK], idx % BITS_PER_BLOCK);
                    }
                }
            }
        }};
    }

    // 遍历全部 inode
    for ino in 1..MAX_INODES {
        let alloc = bit_get(&c.imap, ino);
        let mut inode = [0u8; INODE_SIZE];
        let ok = read_inode(major, ino as u32, &mut inode);
        let typ = if ok { inode_type(&inode) } else { 0 };
        if alloc && (!ok || typ == 0) {
            crate::uart_send_str("[sfs-check] inode 已分配但无效: ");
            crate::print_dec(ino as u64);
            crate::uart_send_str("\n");
            errs += 1;
        }
        if !alloc && ok && typ != 0 {
            crate::uart_send_str("[sfs-check] inode 有效但未分配: ");
            crate::print_dec(ino as u64);
            crate::uart_send_str("\n");
            errs += 1;
        }
        if !alloc || !ok || typ == 0 {
            continue;
        }
        // 校验并标记块引用（宏 check_blk 在函数开头定义）
        let size = inode_size(&inode) as usize;
        let nblocks = size.div_ceil(BLK_SIZE).min(MAX_BLOCKS);
        for logical in 0..nblocks {
            let blk = bmap(major, &inode, logical);
            check_blk!(ino, blk);
        }
        // 中间层块引用
        let ind = inode_indirect(&inode);
        if ind != 0 {
            check_blk!(ino, ind);
            let mut buf = [0u8; BLK_SIZE];
            if meta_read(major, ind as usize, &mut buf) {
                for i in 0..NINDIRECT {
                    let b = get_u32(&buf, i * 4);
                    check_blk!(ino, b);
                }
            }
        }
        let db = inode_double(&inode);
        if db != 0 {
            check_blk!(ino, db);
            let mut buf = [0u8; BLK_SIZE];
            if meta_read(major, db as usize, &mut buf) {
                for i in 0..NINDIRECT {
                    let l1blk = get_u32(&buf, i * 4);
                    if l1blk != 0 {
                        check_blk!(ino, l1blk);
                        let mut b2 = [0u8; BLK_SIZE];
                        if meta_read(major, l1blk as usize, &mut b2) {
                            for j in 0..NINDIRECT {
                                let b = get_u32(&b2, j * 4);
                                check_blk!(ino, b);
                            }
                        }
                    }
                }
            }
        }
        let tb = inode_triple(&inode);
        if tb != 0 {
            check_blk!(ino, tb);
            let mut b1 = [0u8; BLK_SIZE];
            if meta_read(major, tb as usize, &mut b1) {
                for i in 0..NINDIRECT {
                    let l1blk = get_u32(&b1, i * 4);
                    if l1blk != 0 {
                        check_blk!(ino, l1blk);
                        let mut b2 = [0u8; BLK_SIZE];
                        if meta_read(major, l1blk as usize, &mut b2) {
                            for j in 0..NINDIRECT {
                                let l2blk = get_u32(&b2, j * 4);
                                if l2blk != 0 {
                                    check_blk!(ino, l2blk);
                                    let mut b3 = [0u8; BLK_SIZE];
                                    if meta_read(major, l2blk as usize, &mut b3) {
                                        for k in 0..NINDIRECT {
                                            let b = get_u32(&b3, k * 4);
                                            check_blk!(ino, b);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // 目录项指向的 ino 必须已分配
        if typ == T_DIR {
            for i in 0..NDIRECT {
                let blk = inode_direct(&inode, i);
                if blk == 0 {
                    break;
                }
                let mut buf = [0u8; BLK_SIZE];
                if meta_read(major, blk as usize, &mut buf) {
                    for e in (0..BLK_SIZE).step_by(DIRENT_SIZE) {
                        let eino = get_u32(&buf, e);
                        if eino != 0 {
                            if (eino as usize) >= MAX_INODES || !bit_get(&c.imap, eino as usize) {
                                crate::uart_send_str("[sfs-check] dir ");
                                crate::print_dec(ino as u64);
                                crate::uart_send_str(" 存在悬空目录项 -> ino ");
                                crate::print_dec(eino as u64);
                                crate::uart_send_str("\n");
                                errs += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    // 孤儿块：位图已分配但无 inode 引用
    for idx in 0..ndata {
        if bmap_get(c, idx) && !bit_get(&refs[idx / BITS_PER_BLOCK], idx % BITS_PER_BLOCK) {
            crate::uart_send_str("[sfs-check] 孤儿已分配块 ");
            crate::print_dec((c.data_start as usize + idx) as u64);
            crate::uart_send_str("\n");
            errs += 1;
        }
    }

    // 根 inode
    let root = c.root_ino;
    if root != 1 || fs_type_unlocked(major, root) != T_DIR {
        crate::uart_send_str("[sfs-check] 根 inode 无效\n");
        errs += 1;
    }

    crate::uart_send_str("[sfs-check] 完成, 错误数=");
    crate::print_dec(errs as u64);
    crate::uart_send_str("\n");
    if errs == 0 {
        0
    } else {
        -1
    }
}
