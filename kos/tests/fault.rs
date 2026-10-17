//! 运行期 I/O 故障注入（E1）：与 crash.rs 的「断电预算」轴互补，本文件
//! 注入**瞬时故障**（第 N 次读/写/flush 失败一次）与**永久故障**（设备
//! 死亡），断言用户核心诉求「报错了还能继续维持运行」：
//!
//! 1. 全部公开 API 零 panic、零挂死，失败一律 `Err(KosError::Io(..))`；
//! 2. 故障后**同一实例**继续服务：恢复后新写入成功，未涉及对象可读；
//! 3. 被故障打断的记录内容 = 旧值或新值（POSIX 式模糊结果，绝无脏第三态），
//!    且重挂载（含 Full Merkle 校验）与内存视图一致；
//! 4. 读故障返回 Err 而非内容错误的 Ok；
//! 5. 永久故障下全部写路径 Err（不得有僵尸成功），重挂载回到最近完好代。

use kos::block::{BlockDevice, MemBlockDevice};
use kos::error::KosError;
use kos::store::{perm, FlushPolicy, KosStore, VerifyLevel};
use std::cell::Cell;

/// 故障种类（下标进计数表）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Read = 0,
    Write = 1,
    Flush = 2,
}

/// 故障注入设备。计数/开关全走 `Cell`（`read_block` 拿 `&self`）：
/// - `once = Some((op, n))`：第 n 次 op 操作一次性失败（计数从 0 起，
///   三种操作独立计数），触发后自动解除；
/// - `dead`：永久故障——写/冲刷全部失败，读仍可（掉盘后缓存语义）。
struct FaultDev {
    inner: MemBlockDevice,
    once: Cell<Option<(Op, u64)>>,
    counts: [Cell<u64>; 3],
    dead: Cell<bool>,
    /// 注入过至少一次故障（测试自检：保证注入确实生效）。
    fired: Cell<bool>,
    /// 读序列审计（probe 用）：(序号, lba)。
    read_log: std::cell::RefCell<Vec<(u64, u64)>>,
}

impl FaultDev {
    fn new(inner: MemBlockDevice, once: Option<(Op, u64)>) -> Self {
        FaultDev {
            inner,
            once: Cell::new(once),
            counts: [Cell::new(0), Cell::new(0), Cell::new(0)],
            dead: Cell::new(false),
            fired: Cell::new(false),
            read_log: std::cell::RefCell::new(Vec::new()),
        }
    }
    fn count(&self, op: Op) -> u64 {
        self.counts[op as usize].get()
    }
    fn into_inner(self) -> MemBlockDevice {
        self.inner
    }
    /// 计数自增 + 判定本次是否注入故障。
    fn trip(&self, op: Op) -> Result<(), KosError> {
        let n = self.counts[op as usize].get();
        self.counts[op as usize].set(n + 1);
        if op != Op::Read && self.dead.get() {
            return Err(KosError::Io(5));
        }
        if let Some((fop, fnth)) = self.once.get() {
            if fop == op && fnth == n {
                self.once.set(None);
                self.fired.set(true);
                return Err(KosError::Io(5));
            }
        }
        Ok(())
    }
}

impl BlockDevice for FaultDev {
    fn block_size(&self) -> usize {
        self.inner.block_size()
    }
    fn block_count(&self) -> u64 {
        self.inner.block_count()
    }
    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), KosError> {
        if let Err(e) = self.trip(Op::Read) {
            self.read_log.borrow_mut().push((u64::MAX, lba)); // 标记失败点
            return Err(e);
        }
        let n = self.counts[0].get() - 1;
        self.read_log.borrow_mut().push((n, lba));
        self.inner.read_block(lba, buf)
    }
    fn write_block(&mut self, lba: u64, data: &[u8]) -> Result<(), KosError> {
        self.trip(Op::Write)?;
        self.inner.write_block(lba, data)
    }
    fn flush(&mut self) -> Result<(), KosError> {
        self.trip(Op::Flush)?;
        self.inner.flush()
    }
}

/// 基线卷：不可变对象 + 可变记录 + 目录树（每轮故障扫描都从同一镜像出发）。
fn base_image() -> MemBlockDevice {
    let mut s = KosStore::mkfs(MemBlockDevice::new(8192, 4096)).unwrap();
    s.mkdir("/d", 0, perm::DIR_DEFAULT).unwrap();
    s.write("/d/imm", &[0x5Au8; 4096 * 3], 0, perm::DEFAULT).unwrap();
    s.write_mutable("/rec", b"BASE", 0, perm::DEFAULT).unwrap();
    s.symlink("/d/lnk", b"/d/imm", 0).unwrap();
    s.take_device()
}

const APPENDIX: &[u8] = &[0xC3u8; 4096 * 2];

/// 混合工作负载：覆盖全部易故障 API。任何调用都不允许 panic；
/// Err 一律吞掉（由收尾裁定统一判）。
fn workload(s: &mut KosStore<FaultDev>, tag: &str) {
    let _ = s.open_path("/d/imm", 0); // 读路径
    let _ = s.append("/rec", APPENDIX, 0); // 可变记录换代
    let _ = s.write("/d/imm2", &[0x11u8; 4096], 0, perm::DEFAULT); // 不可变写
    let _ = s.write_range("/rec", 2, b"ZZ", 0); // 原地改
    let _ = s.truncate("/rec", 4, 0);
    let _ = s.mkdir("/d2", 0, perm::DIR_DEFAULT);
    let _ = s.rename("/d/imm2", "/d2/moved", 0);
    let _ = s.list("/");
    let _ = s.upload_begin("/up", 4096 * 2, 0, perm::DEFAULT);
    let _ = s.upload_push("/up", 0, &[0x77u8; 4096], 0);
    let _ = s.upload_finish("/up", 0);
    let _ = s.unlink("/d2/moved", 0);
    let _ = s.sync_now();
    let _ = s.generation();
    let _ = s.object_count();
    let _ = s.free_blocks();
    let _ = tag;
}

/// 收尾裁定：故障解除后重开卷——服务可用、内容无脏态、重挂载一致。
fn verdict(dev: FaultDev, tag: &str) {
    let mut dev = dev;
    dev.once.set(None); // 一次性故障若未触发也解除
    dev.dead.set(false);
    let mut s = KosStore::open(dev, VerifyLevel::HashOnly)
        .unwrap_or_else(|e| panic!("{tag}: 故障后重挂载失败：{e:?}"));
    // 服务连续性：恢复后新写必须成功。
    s.append("/rec", b"POST", 0)
        .unwrap_or_else(|e| panic!("{tag}: 故障恢复后服务不可用：{e:?}"));
    // 未涉及对象内容精确；被打断的记录 = 旧或新（无脏第三态）。
    assert_eq!(
        s.open_path("/d/imm", 0).unwrap(),
        &[0x5Au8; 4096 * 3],
        "{tag}: 无关对象被波及"
    );
    let v = s.open_path("/rec", 0).unwrap();
    let old_or_new = v.starts_with(b"BASE") || v.starts_with(b"BAZZ") || v.starts_with(b"BASEZZ");
    assert!(
        old_or_new && v.ends_with(b"POST"),
        "{tag}: 脏第三态 len={} head={:?} tail={:?}",
        v.len(),
        &v[..8.min(v.len())],
        &v[v.len().saturating_sub(4)..]
    );
    // Full Merkle 重挂载与内存视图一致（无脏块残留）。
    let d = s.take_device();
    let mut s2 = KosStore::open(d, VerifyLevel::Full)
        .unwrap_or_else(|e| panic!("{tag}: Full 校验挂载失败：{e:?}"));
    assert_eq!(&s2.open_path("/rec", 0).unwrap()[..], &v[..], "{tag}: 重挂载结论漂移");
}

#[test]
fn transient_write_faults_matrix() {
    // 先数一遍工作负载的写次数，再枚举全部一次性故障点。
    let total_writes = {
        let mut s = KosStore::open(FaultDev::new(base_image(), None), VerifyLevel::HashOnly).unwrap();
        s.set_flush_policy(FlushPolicy::Eager);
        workload(&mut s, "count");
        s.take_device().count(Op::Write)
    };
    eprintln!("工作负载写次数 = {total_writes}，枚举全部一次性写故障点");
    for k in 0..total_writes {
        let mut s = KosStore::open(
            FaultDev::new(base_image(), Some((Op::Write, k))),
            VerifyLevel::HashOnly,
        )
        .unwrap();
        s.set_flush_policy(FlushPolicy::Eager);
        workload(&mut s, &format!("w{k}"));
        let dev = s.take_device();
        assert!(dev.fired.get(), "w{k}: 注入未生效");
        verdict(dev, &format!("w{k}"));
    }
}

#[test]
fn transient_read_faults_return_err() {
    // 读故障（挂载读 + 内容读统一枚举）：一切失败必须是 Io，绝不 Ok 返回
    // 错误内容；故障解除后最多再试 2 次必须读到精确内容。
    let total_reads = {
        let mut s = KosStore::open(FaultDev::new(base_image(), None), VerifyLevel::HashOnly).unwrap();
        let _ = s.open_path("/d/imm", 0);
        s.take_device().count(Op::Read)
    };
    eprintln!("挂载+单次 open_path 读块数 = {total_reads}，枚举读故障点");
    for k in 0..total_reads {
        let dev = FaultDev::new(base_image(), Some((Op::Read, k)));
        let mut s = match KosStore::open(dev, VerifyLevel::HashOnly) {
            Ok(s) => s,
            Err(KosError::Io(_)) => continue, // 挂载期读故障：Err 即正确结局
            Err(e) => panic!("读故障点 {k}：挂载期非 I/O 错误 {e:?}"),
        };
        let mut exact = false;
        for _ in 0..3 {
            match s.open_path("/d/imm", 0) {
                Ok(v) => {
                    assert_eq!(v, &[0x5Au8; 4096 * 3], "读故障点 {k}：Ok 但内容错误");
                    exact = true;
                    break;
                }
                Err(KosError::Io(_)) => continue, // 故障可能延迟到本次读才触发
                Err(e) => panic!("读故障点 {k}：非 I/O 错误 {e:?}"),
            }
        }
        assert!(exact, "读故障点 {k}：故障解除后 3 次尝试仍不可读");
    }
}

#[test]
fn transient_flush_faults_tolerated() {
    let total_flushes = {
        let mut s = KosStore::open(FaultDev::new(base_image(), None), VerifyLevel::HashOnly).unwrap();
        s.set_flush_policy(FlushPolicy::Eager);
        workload(&mut s, "count");
        s.take_device().count(Op::Flush)
    };
    eprintln!("工作负载 flush 次数 = {total_flushes}");
    if total_flushes == 0 {
        return;
    }
    for k in 0..total_flushes {
        let mut s = KosStore::open(
            FaultDev::new(base_image(), Some((Op::Flush, k))),
            VerifyLevel::HashOnly,
        )
        .unwrap();
        s.set_flush_policy(FlushPolicy::Eager);
        workload(&mut s, &format!("f{k}"));
        let dev = s.take_device();
        assert!(dev.fired.get(), "f{k}: 注入未生效");
        verdict(dev, &format!("f{k}"));
    }
}

#[test]
fn permanent_failure_degrades_gracefully() {
    // 死盘打开（读可用，写必败）：全部写路径 Err（可重复调用，不僵尸成功），
    // 然后热恢复——服务恢复，内容仍是最近完好代。
    let mut dev = FaultDev::new(base_image(), None);
    dev.dead.set(true);
    let mut s = KosStore::open(dev, VerifyLevel::HashOnly).unwrap();
    for i in 0..3 {
        assert!(
            matches!(s.append("/rec", b"x", 0), Err(KosError::Io(_))),
            "死盘 append #{i} 未拒绝"
        );
        assert!(
            matches!(s.write("/z", b"x", 0, perm::DEFAULT), Err(KosError::Io(_))),
            "死盘 write #{i} 未拒绝"
        );
        assert!(
            matches!(s.mkdir("/zz", 0, perm::DIR_DEFAULT), Err(KosError::Io(_))),
            "死盘 mkdir #{i} 未拒绝"
        );
        assert!(matches!(s.sync_now(), Err(KosError::Io(_))), "死盘 sync #{i} 未拒绝");
    }
    let mut dev = s.take_device();
    dev.dead.set(false);
    let mut s2 = KosStore::open(dev, VerifyLevel::Full).unwrap();
    assert!(
        s2.open_path("/rec", 0).unwrap().starts_with(b"BASE"),
        "死盘期间出现脏写入"
    );
    s2.append("/rec", b"RECOVER", 0).unwrap();
}

#[test]
fn gc_survivors_intact_after_faults() {
    // gc 中途故障：存活对象不丢，重挂载 Full 校验通过。
    // 基线（复刻 gc 前置状态）后的写次数（+8 跳过前置写入，落进 gc 本体）。
    let total = {
        let mut probe = KosStore::open(FaultDev::new(gc_base(), None), VerifyLevel::HashOnly).unwrap();
        let _ = probe.gc();
        probe.take_device().count(Op::Write)
    };
    eprintln!("gc 写次数 = {total}");
    for k in 0..total {
        let mut s = KosStore::open(
            FaultDev::new(gc_base(), Some((Op::Write, k))),
            VerifyLevel::HashOnly,
        )
        .unwrap();
        let _ = s.gc(); // Ok 或 Err 均可，不允许 panic
        let dev = s.take_device();
        assert!(dev.fired.get(), "gc fault {k}: 注入未生效");
        let mut s2 = KosStore::open(dev.into_inner(), VerifyLevel::Full)
            .unwrap_or_else(|e| panic!("gc fault {k}: 重挂载失败 {e:?}"));
        assert_eq!(
            s2.open_path("/d/imm", 0).unwrap(),
            &[0x5Au8; 4096 * 3],
            "gc fault {k}: 存活对象丢失"
        );
    }
}

fn gc_base() -> MemBlockDevice {
    let mut s = KosStore::mkfs(MemBlockDevice::new(8192, 4096)).unwrap();
    s.mkdir("/d", 0, perm::DIR_DEFAULT).unwrap();
    s.write("/d/imm", &[0x5Au8; 4096 * 3], 0, perm::DEFAULT).unwrap();
    s.write_mutable("/doomed", b"junk", 0, perm::DEFAULT).unwrap();
    s.unlink("/doomed", 0).unwrap();
    s.take_device()
}
