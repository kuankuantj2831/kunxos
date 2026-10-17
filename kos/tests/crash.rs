//! 断电注入测试矩阵：任意第 N 次块写后"掉电"，重挂载必须：
//! ①不 panic 不挂死；②对象内容 = 旧值或新值（绝无脏第三态）；③二次
//! 重挂载稳定。写序正确性（先数据后头、双槽提交）由此矩阵背书。

use kos::block::{BlockDevice, MemBlockDevice};
use kos::error::KosError;
use kos::store::{perm, KosStore, VerifyLevel};

/// 断电模拟：写预算耗尽后所有写/冲刷失败（写入未落盘）。
struct CrashDev {
    inner: MemBlockDevice,
    writes_left: u64,
    used: u64,
    dead: bool,
}

impl CrashDev {
    fn new(inner: MemBlockDevice, budget: u64) -> Self {
        CrashDev { inner, writes_left: budget, used: 0, dead: false }
    }
    fn used_writes(&self) -> u64 {
        self.used
    }
    fn into_inner(self) -> MemBlockDevice {
        self.inner
    }
}

impl BlockDevice for CrashDev {
    fn block_size(&self) -> usize {
        self.inner.block_size()
    }
    fn block_count(&self) -> u64 {
        self.inner.block_count()
    }
    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), KosError> {
        self.inner.read_block(lba, buf)
    }
    fn write_block(&mut self, lba: u64, data: &[u8]) -> Result<(), KosError> {
        if self.dead {
            return Err(KosError::Io(5));
        }
        if self.writes_left == 0 {
            self.dead = true;
            return Err(KosError::Io(5));
        }
        self.writes_left -= 1;
        self.used += 1;
        self.inner.write_block(lba, data)
    }
    fn flush(&mut self) -> Result<(), KosError> {
        if self.dead {
            return Err(KosError::Io(5));
        }
        Ok(())
    }
}

const DATA: &[u8] = &[0xA5u8; 4096 * 6];

fn prep_base() -> MemBlockDevice {
    let mut s = KosStore::mkfs(MemBlockDevice::new(4096, 4096)).unwrap();
    s.write_mutable("/rec", b"BASE", 0, perm::DEFAULT).unwrap();
    s.take_device()
}

fn assert_powercut_ok(dev: MemBlockDevice, tag: &str) {
    let mut s = KosStore::open(dev, VerifyLevel::HashOnly)
        .unwrap_or_else(|e| panic!("{tag}: 掉电后挂载失败：{e:?}"));
    let v = s
        .open_path("/rec", 0)
        .unwrap_or_else(|e| panic!("{tag}: /rec 不可读：{e:?}"));
    let is_old = v == b"BASE";
    let is_new = v.len() == 4 + DATA.len()
        && v[..4] == *b"BASE"
        && v[4..].iter().all(|&b| b == 0xA5);
    assert!(
        is_old || is_new,
        "{tag}: 掉电后出现脏状态（len={}，头 8 字节={:?}）",
        v.len(),
        &v[..core::cmp::min(8, v.len())]
    );
    let d = s.take_device();
    let mut s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    let v2 = s2.open_path("/rec", 0).unwrap();
    assert_eq!(v2, v, "{tag}: 二次重挂载结论漂移");
}

/// 计数工作负载总写块数（append DATA）。
fn count_append_writes() -> u64 {
    let mut s = KosStore::open(CrashDev::new(prep_base(), u64::MAX), VerifyLevel::HashOnly)
        .unwrap();
    s.append("/rec", DATA, 0).unwrap();
    s.take_device().used_writes()
}

#[test]
fn crash_during_append_matrix() {
    let total = count_append_writes();
    eprintln!("append 工作负载总写块数 = {total}，枚举全部断点");
    for cut in 0..=total {
        let mut s = KosStore::open(CrashDev::new(prep_base(), cut), VerifyLevel::HashOnly)
            .unwrap();
        let _ = s.append("/rec", DATA, 0); // Ok（预算恰好够）或 Err（断电）
        let cd = s.take_device();
        assert_powercut_ok(cd.into_inner(), &format!("append cut={cut}"));
    }
}

#[test]
fn crash_during_write_range_matrix() {
    // 预置长记录，然后原地改写中段。
    let base: Vec<u8> = (0..4096u32 * 16).map(|i| (i % 256) as u8).collect();
    
    let total = {
        let mut s0 = KosStore::mkfs(MemBlockDevice::new(4096, 4096)).unwrap();
        s0.write_mutable("/r", &base, 0, perm::DEFAULT).unwrap();
        let img = s0.take_device();
        let mut s = KosStore::open(CrashDev::new(img, u64::MAX), VerifyLevel::HashOnly).unwrap();
        s.write_range("/r", 4096 * 7 + 100, &[0xEE; 900], 0).unwrap();
        s.take_device().used_writes()
    };
    eprintln!("write_range 总写块数 = {total}");
    for cut in 0..=total {
        let mut s0 = KosStore::mkfs(MemBlockDevice::new(4096, 4096)).unwrap();
        s0.write_mutable("/r", &base, 0, perm::DEFAULT).unwrap();
        let img = s0.take_device();
        let mut s = KosStore::open(CrashDev::new(img, cut), VerifyLevel::HashOnly).unwrap();
        let _ = s.write_range("/r", 4096 * 7 + 100, &[0xEE; 900], 0);
        // 断电复验：内容 = 原样或改写后，且 Full 校验必须过。
        let cd = s.take_device();
        let mut s2 = KosStore::open(cd.into_inner(), VerifyLevel::Full)
            .unwrap_or_else(|e| panic!("cut={cut}: 挂载失败 {e:?}"));
        let v = s2.open_path("/r", 0).unwrap();
        let expect_old = &base;
        let expect_new = {
            let mut n = base.clone();
            n[4096 * 7 + 100..4096 * 7 + 100 + 900].copy_from_slice(&[0xEE; 900]);
            n
        };
        assert!(
            v == *expect_old || v == expect_new,
            "cut={cut}: write_range 断电后脏状态（len={}）",
            v.len()
        );
    }
}

#[test]
fn crash_during_truncate_and_mkdir_matrix() {
    // truncate 收缩。
    let base: Vec<u8> = (0..4096u32 * 8).map(|i| (i % 199) as u8).collect();
    let total = {
        let mut s0 = KosStore::mkfs(MemBlockDevice::new(4096, 4096)).unwrap();
        s0.write_mutable("/t", &base, 0, perm::DEFAULT).unwrap();
        let img = s0.take_device();
        let mut s = KosStore::open(CrashDev::new(img, u64::MAX), VerifyLevel::HashOnly).unwrap();
        s.truncate("/t", 4096 * 3, 0).unwrap();
        s.take_device().used_writes()
    };
    for cut in 0..=total {
        let mut s0 = KosStore::mkfs(MemBlockDevice::new(4096, 4096)).unwrap();
        s0.write_mutable("/t", &base, 0, perm::DEFAULT).unwrap();
        let img = s0.take_device();
        let mut s = KosStore::open(CrashDev::new(img, cut), VerifyLevel::HashOnly).unwrap();
        let _ = s.truncate("/t", 4096 * 3, 0);
        let cd = s.take_device();
        let mut s2 = KosStore::open(cd.into_inner(), VerifyLevel::Full)
            .unwrap_or_else(|e| panic!("trunc cut={cut}: {e:?}"));
        let v = match s2.open_path("/t", 0) {
            Ok(v) => v,
            Err(e) => panic!("trunc cut={cut} 读取失败: {e:?}"),
        };
        assert!(
            v == base || v == base[..4096 * 3],
            "trunc cut={cut}: 脏状态 len={}",
            v.len()
        );
    }
}

#[test]
fn superblock_corruption_self_heals() {
    // 超级块整块打花：挂载必须成功（自愈重建），数据完好。
    let mut s0 = KosStore::mkfs(MemBlockDevice::new(4096, 4096)).unwrap();
    s0.write_mutable("/f", b"survivor", 0, perm::DEFAULT).unwrap();
    let mut dev = s0.take_device();
    let garbage = [0xEEu8; 4096];
    use kos::block::BlockDevice;
    dev.write_block(0, &garbage).unwrap();
    let mut s = KosStore::open(dev, VerifyLevel::Full).unwrap();
    assert_eq!(s.open_path("/f", 0).unwrap(), b"survivor");
    // 后续写入 + 重挂载照常。
    s.append("/f", b"!", 0).unwrap();
    let d = s.take_device();
    let mut s2 = KosStore::open(d, VerifyLevel::Full).unwrap();
    assert_eq!(s2.open_path("/f", 0).unwrap(), b"survivor!");
}

#[test]
fn upload_resume_after_powercut() {
    use kos::store::perm;
    const TOTAL: u64 = 4096 * 40;

    // 会话 + 前半推送。
    let mut s = KosStore::mkfs(MemBlockDevice::new(8192, 4096)).unwrap();
    s.upload_begin("/pkg", TOTAL, 0, perm::DEFAULT).unwrap();
    let half = TOTAL as usize / 2;
    let chunk = [0x77u8; 4096];
    let mut off = 0usize;
    while off < half {
        s.upload_push("/pkg", off as u64, &chunk, 0).unwrap();
        off += 4096;
    }
    let (_, received, _) = s.upload_status("/pkg", 0).unwrap();
    assert_eq!(received, 20);

    // "断电"：直接丢弃句柄（等价关机，未走任何清理）。
    let dev = s.take_device();

    // 重启：会话进度保留，续传后半。
    let mut s2 = KosStore::open(dev, VerifyLevel::Full).unwrap();
    let (total, received2, contiguous) = s2.upload_status("/pkg", 0).unwrap();
    assert_eq!((total, received2, contiguous), (TOTAL, 20, 20));
    let mut off = half;
    while off < TOTAL as usize {
        s2.upload_push("/pkg", off as u64, &chunk, 0).unwrap();
        off += 4096;
    }
    // 未完成时 finish 拒绝 → 全部到齐后原子发布。
    s2.upload_finish("/pkg", 0).unwrap();
    let data = s2.open_path("/pkg", 0).unwrap();
    assert_eq!(data.len(), TOTAL as usize);
    assert!(data.iter().all(|&b| b == 0x77));
    // 会话对象已清理。
    assert!(s2.lookup_entry_pub("/pkg.kosup.meta").unwrap().is_none());

    // 断电注入矩阵：推送中途任意断点 → 重启续传 → 数据完好。
    let total_writes = {
        let mut p = KosStore::open(
            CrashDev::new(prep_upload_base(), u64::MAX),
            VerifyLevel::HashOnly,
        )
        .unwrap();
        p.upload_push("/pkg", 0, &[0x99u8; 4096 * 4], 0).unwrap();
        p.take_device().used_writes()
    };
    for cut in 0..=total_writes {
        let mut s3 = KosStore::open(
            CrashDev::new(prep_upload_base(), cut),
            VerifyLevel::HashOnly,
        )
        .unwrap();
        let _ = s3.upload_push("/pkg", 0, &[0x99u8; 4096 * 4], 0);
        let cd = s3.take_device();
        // 重启：begin 幂等（保留进度），补齐后 finish 必须成功。
        let mut s4 = KosStore::open(cd.into_inner(), VerifyLevel::Full).unwrap();
        s4.upload_begin("/pkg", TOTAL, 0, perm::DEFAULT).unwrap();
        let full = [0x99u8; 4096 * 4];
        s4.upload_push("/pkg", 0, &full, 0).unwrap();
        let mut off = 4096 * 4;
        while off < TOTAL as usize {
            s4.upload_push("/pkg", off as u64, &full[..4096], 0).unwrap();
            off += 4096;
        }
        s4.upload_finish("/pkg", 0).unwrap_or_else(|e| panic!("cut={cut}: {e:?}"));
        assert_eq!(s4.open_path("/pkg", 0).unwrap().len(), TOTAL as usize);
    }
}

fn prep_upload_base() -> MemBlockDevice {
    use kos::store::perm;
    let mut s = KosStore::mkfs(MemBlockDevice::new(8192, 4096)).unwrap();
    s.upload_begin("/pkg", 4096 * 40, 0, perm::DEFAULT).unwrap();
    s.take_device()
}
