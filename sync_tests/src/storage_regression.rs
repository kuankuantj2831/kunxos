use crate::host::HostFs;
use crate::sync::fs::Fs;
use crate::sync::store::{Rec, Store};
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestDir(std::path::PathBuf);
impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self(std::env::temp_dir().join(format!("kunx-storage-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))))
    }
}
impl Drop for TestDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}
fn record(key: u8) -> Rec {
    let mut r = Rec::new();
    r.key[0] = key;
    r.key_len = 1;
    r
}

#[test]
fn offset_writes_preserve_prefix_and_suffix() {
    let dir = TestDir::new();
    let mut fs = HostFs::new(dir.0.clone());
    assert!(fs.write_file(b"blob", b"ABCDEFGH"));
    assert!(fs.write_file_at(b"blob", 3, b"xy"));
    assert!(fs.write_file_at(b"blob", 8, b"IJ"));
    let mut out = [0u8; 16];
    assert_eq!(fs.read_file(b"blob", &mut out), 10);
    assert_eq!(&out[..10], b"ABCxyFGHIJ");
}

#[test]
fn updated_records_survive_multiple_save_and_reload_cycles() {
    let dir = TestDir::new();
    let mut fs = HostFs::new(dir.0.clone());
    let mut blob = [0u8; 256];
    let mut store = Store::new(&mut blob, b"idx", b"blob");
    assert!(store.upsert(record(b'a'), b"old-a", true));
    assert!(store.upsert(record(b'b'), b"keep-b", true));
    assert!(store.upsert(record(b'c'), b"", true));
    assert!(store.save(&mut fs));
    assert!(store.upsert(record(b'a'), b"longer-new-a", true));
    assert!(store.save(&mut fs));
    let mut blob2 = [0u8; 256];
    let mut reload = Store::new(&mut blob2, b"idx", b"blob");
    assert!(reload.load(&mut fs));
    assert_eq!(reload.blob_of(reload.find(b"a").unwrap()), Some(&b"longer-new-a"[..]));
    assert_eq!(reload.blob_of(reload.find(b"b").unwrap()), Some(&b"keep-b"[..]));
    assert!(reload.save(&mut fs));
    let mut blob3 = [0u8; 256];
    let mut again = Store::new(&mut blob3, b"idx", b"blob");
    assert!(again.load(&mut fs));
    assert_eq!(again.blob_of(again.find(b"a").unwrap()), Some(&b"longer-new-a"[..]));
    assert_eq!(again.blob_of(again.find(b"b").unwrap()), Some(&b"keep-b"[..]));
}

#[test]
fn invalid_blob_range_does_not_destroy_saved_files() {
    let dir = TestDir::new();
    let mut fs = HostFs::new(dir.0.clone());
    let mut blob = [0u8; 64];
    let mut store = Store::new(&mut blob, b"idx", b"blob");
    assert!(store.upsert(record(b'a'), b"safe", true));
    assert!(store.save(&mut fs));
    store.recs[0].boff = u32::MAX;
    assert!(!store.save(&mut fs));
    let mut blob2 = [0u8; 64];
    let mut reload = Store::new(&mut blob2, b"idx", b"blob");
    assert!(reload.load(&mut fs));
    assert_eq!(reload.blob_of(0), Some(&b"safe"[..]));
}
