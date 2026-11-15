//! 掉电安全回归：Store::save 在任意写操作之间中断（含半写）后，磁盘上必须仍有
//! 一代完整、校验通过的数据，加载结果只能是"保存前"或"保存后"两种状态之一。
use crate::host::MemFs;
use crate::sync::fs::Fs;
use crate::sync::model::FLAG_ON_SERVER;
use crate::sync::store::{Rec, Store, IDX_MAGIC};

fn record(key: u8) -> Rec {
    let mut r = Rec::new();
    r.key[0] = key;
    r.key_len = 1;
    r
}

/// 故障注入文件系统：前 `budget` 次修改操作正常；之后的修改全部失败。
/// `torn` 为真时，第一次失败的写操作只落盘前一半数据（模拟写到一半掉电）。
struct FaultFs {
    inner: MemFs,
    budget: usize,
    torn: bool,
    torn_used: bool,
    ops: usize,
}
enum Gate { Full, Half, Dead }
impl FaultFs {
    fn new(inner: MemFs, budget: usize, torn: bool) -> Self {
        FaultFs { inner, budget, torn, torn_used: false, ops: 0 }
    }
    fn gate(&mut self) -> Gate {
        if self.ops < self.budget {
            self.ops += 1;
            Gate::Full
        } else if self.torn && !self.torn_used {
            self.torn_used = true;
            Gate::Half
        } else {
            Gate::Dead
        }
    }
}
impl Fs for FaultFs {
    fn read_file(&mut self, path: &[u8], buf: &mut [u8]) -> isize { self.inner.read_file(path, buf) }
    fn write_file(&mut self, path: &[u8], data: &[u8]) -> bool {
        match self.gate() {
            Gate::Full => self.inner.write_file(path, data),
            Gate::Half => { self.inner.write_file(path, &data[..data.len() / 2]); false }
            Gate::Dead => false,
        }
    }
    fn write_file_at(&mut self, path: &[u8], off: u32, data: &[u8]) -> bool {
        match self.gate() {
            Gate::Full => self.inner.write_file_at(path, off, data),
            Gate::Half => { self.inner.write_file_at(path, off, &data[..data.len() / 2]); false }
            Gate::Dead => false,
        }
    }
    fn delete_file(&mut self, path: &[u8]) -> bool {
        match self.gate() {
            Gate::Full => self.inner.delete_file(path),
            _ => false,
        }
    }
    fn file_exists(&mut self, path: &[u8]) -> bool { self.inner.file_exists(path) }
}

fn clone_fs(fs: &MemFs) -> MemFs { MemFs { files: fs.files.clone() } }

/// 读出全部活动记录 (key, blob)，按 key 排序，便于比较状态。
fn snapshot(s: &Store) -> Vec<(u8, Vec<u8>)> {
    let mut v = Vec::new();
    for i in 0..s.n {
        if s.recs[i].is_deleted() { continue; }
        v.push((s.recs[i].key[0], s.blob_of(i).map(|b| b.to_vec()).unwrap_or_default()));
    }
    v.sort();
    v
}

/// 建立"状态一"：a、b、c 三条记录并保存两次（使磁盘处于代际格式）。
fn state_one() -> (MemFs, Vec<(u8, Vec<u8>)>) {
    let mut fs = MemFs::new();
    let mut buf = [0u8; 512];
    let mut s = Store::new(&mut buf, b"idx", b"blob");
    assert!(s.load(&mut fs));
    assert!(s.upsert(record(b'a'), b"alpha-one", true));
    assert!(s.upsert(record(b'b'), b"bravo-one", true));
    assert!(s.upsert(record(b'c'), b"charlie-one", true));
    assert!(s.save(&mut fs));
    assert!(s.save(&mut fs));
    let snap = snapshot(&s);
    (fs, snap)
}

/// 在状态一基础上做"状态二"修改：更新 a（变长）、删除 b、新增 d。
fn apply_state_two(s: &mut Store) {
    assert!(s.upsert(record(b'a'), b"alpha-two-much-longer", true));
    let b = s.find(b"b").unwrap();
    s.remove(b);
    assert!(s.upsert(record(b'd'), b"delta-two", true));
}

#[test]
fn power_loss_at_every_write_step_keeps_old_or_new_state() {
    let (base, state1) = state_one();

    // 先无故障跑一遍，得到"状态二"和修改操作总数。
    let mut probe = FaultFs::new(clone_fs(&base), usize::MAX, false);
    let mut buf = [0u8; 512];
    let mut s = Store::new(&mut buf, b"idx", b"blob");
    assert!(s.load(&mut probe));
    apply_state_two(&mut s);
    assert!(s.save(&mut probe));
    let state2 = snapshot(&s);
    let total_ops = probe.ops;
    assert!(total_ops >= 4, "save should perform several mutating operations");
    assert_ne!(state1, state2);

    for torn in [false, true] {
        for budget in 0..=total_ops {
            let mut ffs = FaultFs::new(clone_fs(&base), budget, torn);
            let mut buf = [0u8; 512];
            let mut s = Store::new(&mut buf, b"idx", b"blob");
            assert!(s.load(&mut ffs));
            apply_state_two(&mut s);
            let in_memory_before = (s.blob_len, s.recs[..s.n].iter().map(|r| r.boff).collect::<Vec<_>>());
            let ok = s.save(&mut ffs);
            if !ok {
                // 保存失败不得改动内存中的记录/Blob 布局
                let after = (s.blob_len, s.recs[..s.n].iter().map(|r| r.boff).collect::<Vec<_>>());
                assert_eq!(in_memory_before, after, "failed save must not touch memory (budget {budget}, torn {torn})");
            }

            // “重启”：只剩磁盘内容
            let mut disk = ffs.inner;
            let mut buf2 = [0u8; 512];
            let mut reload = Store::new(&mut buf2, b"idx", b"blob");
            assert!(reload.load(&mut disk), "must load after power loss (budget {budget}, torn {torn})");
            let got = snapshot(&reload);
            if ok {
                assert_eq!(got, state2, "completed save must be visible (budget {budget}, torn {torn})");
            } else {
                assert!(got == state1 || got == state2,
                    "torn state after power loss (budget {budget}, torn {torn}): {got:?}");
            }
            // 重启后继续保存、再加载仍然正确
            assert!(reload.save(&mut disk));
            let mut buf3 = [0u8; 512];
            let mut again = Store::new(&mut buf3, b"idx", b"blob");
            assert!(again.load(&mut disk));
            assert_eq!(snapshot(&again), got);
        }
    }
}

#[test]
fn corrupt_newest_generation_falls_back_to_previous() {
    let (gen_prev_fs, state1) = state_one();

    // 再保存一次得到更新的一代（同一状态，代号 +1，落在另一个槽位）。
    let mut fs = clone_fs(&gen_prev_fs);
    let mut buf = [0u8; 512];
    let mut s = Store::new(&mut buf, b"idx", b"blob");
    assert!(s.load(&mut fs));
    let prev_slot = s.active_slot().unwrap();
    apply_state_two(&mut s);
    assert!(s.save(&mut fs));
    let new_slot = s.active_slot().unwrap();
    assert_ne!(prev_slot, new_slot);

    // 把上一代文件放回去（模拟"提交后、删除上一代之前"的状态）。
    for ext in ["idx", "blob"] {
        let name = format!("{ext}.{prev_slot}").into_bytes();
        fs.files.insert(name.clone(), gen_prev_fs.files[&name].clone());
    }

    // 损坏最新一代的索引：回退到上一代。
    let mut bad_idx = clone_fs(&fs);
    bad_idx.files.get_mut(format!("idx.{new_slot}").as_bytes()).unwrap()[40] ^= 0xFF;
    let mut buf2 = [0u8; 512];
    let mut r = Store::new(&mut buf2, b"idx", b"blob");
    assert!(r.load(&mut bad_idx));
    assert_eq!(snapshot(&r), state1);

    // 损坏最新一代的 blob：同样回退。
    let mut bad_blob = clone_fs(&fs);
    bad_blob.files.get_mut(format!("blob.{new_slot}").as_bytes()).unwrap()[0] ^= 0xFF;
    let mut buf3 = [0u8; 512];
    let mut r = Store::new(&mut buf3, b"idx", b"blob");
    assert!(r.load(&mut bad_blob));
    assert_eq!(snapshot(&r), state1);

    // 两代都完好：加载最新一代。
    let mut buf4 = [0u8; 512];
    let mut r = Store::new(&mut buf4, b"idx", b"blob");
    assert!(r.load(&mut fs));
    assert!(snapshot(&r).iter().any(|(k, _)| *k == b'd'));
}

#[test]
fn corruption_without_fallback_is_an_error_not_an_empty_store() {
    let (mut fs, _) = state_one();
    let mut buf = [0u8; 512];
    let mut s = Store::new(&mut buf, b"idx", b"blob");
    assert!(s.load(&mut fs));
    let slot = s.active_slot().unwrap();
    fs.files.get_mut(format!("idx.{slot}").as_bytes()).unwrap()[12] ^= 0x01; // 只有一代，且已损坏
    let mut buf2 = [0u8; 512];
    let mut r = Store::new(&mut buf2, b"idx", b"blob");
    assert!(!r.load(&mut fs));
    assert!(!r.loaded);
    assert_eq!(r.n, 0);
}

#[test]
fn legacy_single_file_layout_is_migrated_on_first_save() {
    // 手工构造旧版 <idx> + <blob> 单文件布局。
    let mut fs = MemFs::new();
    let payloads: [(u8, &[u8]); 2] = [(b'x', b"legacy-x"), (b'y', b"legacy-y!")];
    let mut idx = Vec::new();
    idx.extend_from_slice(&IDX_MAGIC.to_le_bytes());
    idx.extend_from_slice(&2u32.to_le_bytes());
    idx.extend_from_slice(&7u32.to_le_bytes());
    let mut blob = Vec::new();
    for (k, data) in payloads {
        let mut r = record(k);
        r.flags = FLAG_ON_SERVER;
        r.boff = blob.len() as u32;
        r.blen = data.len() as u32;
        blob.extend_from_slice(data);
        let mut rb = [0u8; crate::sync::model::REC_SIZE];
        r.to_bytes(&mut rb);
        idx.extend_from_slice(&rb);
    }
    fs.files.insert(b"idx".to_vec(), idx);
    fs.files.insert(b"blob".to_vec(), blob);

    let mut buf = [0u8; 256];
    let mut s = Store::new(&mut buf, b"idx", b"blob");
    assert!(s.load(&mut fs));
    assert_eq!(s.n, 2);
    assert_eq!(s.next_id, 7);
    assert_eq!(snapshot(&s), vec![(b'x', b"legacy-x".to_vec()), (b'y', b"legacy-y!".to_vec())]);
    assert_eq!(s.active_slot(), None);

    assert!(s.save(&mut fs));
    assert!(s.active_slot().is_some());
    assert!(!fs.files.contains_key(b"idx".as_slice()) && !fs.files.contains_key(b"blob".as_slice()),
        "legacy files are removed after the first committed save");

    let mut buf2 = [0u8; 256];
    let mut r = Store::new(&mut buf2, b"idx", b"blob");
    assert!(r.load(&mut fs));
    assert_eq!(r.next_id, 7);
    assert_eq!(snapshot(&r), snapshot(&s));
}

#[test]
fn crash_during_migration_keeps_legacy_data() {
    // 迁移保存在每个写步骤中断：旧版文件必须仍可读（或新代已完整）。
    let mut base = MemFs::new();
    {
        // 用一次保存生成数据，再转换回旧版布局不方便；直接构造一条旧版记录。
        let mut r = record(b'z');
        r.flags = FLAG_ON_SERVER;
        r.boff = 0;
        r.blen = 6;
        let mut idx = Vec::new();
        idx.extend_from_slice(&IDX_MAGIC.to_le_bytes());
        idx.extend_from_slice(&1u32.to_le_bytes());
        idx.extend_from_slice(&3u32.to_le_bytes());
        let mut rb = [0u8; crate::sync::model::REC_SIZE];
        r.to_bytes(&mut rb);
        idx.extend_from_slice(&rb);
        base.files.insert(b"idx".to_vec(), idx);
        base.files.insert(b"blob".to_vec(), b"legacy".to_vec());
    }
    for torn in [false, true] {
        for budget in 0..8 {
            let mut ffs = FaultFs::new(clone_fs(&base), budget, torn);
            let mut buf = [0u8; 64];
            let mut s = Store::new(&mut buf, b"idx", b"blob");
            assert!(s.load(&mut ffs));
            let _ = s.save(&mut ffs);
            let mut disk = ffs.inner;
            let mut buf2 = [0u8; 64];
            let mut r = Store::new(&mut buf2, b"idx", b"blob");
            assert!(r.load(&mut disk), "budget {budget} torn {torn}");
            assert_eq!(snapshot(&r), vec![(b'z', b"legacy".to_vec())], "budget {budget} torn {torn}");
        }
    }
}
