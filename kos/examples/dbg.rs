use kos::block::file::FileBlockDevice;
use kos::store::{perm, KosStore, VerifyLevel};
use std::path::Path;
fn open_img() -> FileBlockDevice {
    let _ = std::fs::remove_file("/tmp/m.img");
    FileBlockDevice::open(Path::new("/tmp/m.img"), Some(51200)).unwrap()
}
fn main() {
    let mut s = KosStore::mkfs(open_img()).unwrap();
    s.mkdir("/data", 0, perm::DIR_DEFAULT).unwrap();
    s.write("/data/blob", &[7u8; 4096*100], 0, perm::DEFAULT).unwrap();
    s.write_mutable("/data/j", b"j0\n", 0, perm::DEFAULT).unwrap();
    for i in 0..20 { s.append("/data/j", format!("e{i} ").as_bytes(), 0).unwrap(); }
    let d = s.take_device();
    let mut s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    println!("m1 fast={}", s2.was_fast_mounted());
    s2.write("/data/after", b"ok", 0, perm::DEFAULT).unwrap();
    let d = s2.take_device();
    let s3 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    println!("m2 fast={}", s3.was_fast_mounted());
}
