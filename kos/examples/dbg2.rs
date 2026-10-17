//! 诊断：fast-mount 在 capacity 场景（2TB）失败在哪一步。
//! 复刻 tests/capacity.rs::sparse_2tb_volume 的序列，重挂载前转储
//! hint / 超级块 / manifest，逐项复算 fast_mount 的判定条件。

use kos::block::file::FileBlockDevice;
use kos::block::BlockDevice;
use kos::format::superblock::KosSuperblock;
use kos::store::persist::MountHint;
use kos::store::{perm, KosStore, VerifyLevel};

fn dump_stage(dev: &FileBlockDevice, tag: &str) {
    let mut b0 = [0u8; 4096];
    dev.read_block(0, &mut b0).unwrap();
    let sb = KosSuperblock::from_bytes(&b0).expect("sb parse");
    let hint = MountHint::from_sb(&sb);
    let mut ma = [0u8; 4096];
    dev.read_block(sb.root_slot_a, &mut ma).unwrap();
    let mut mb = [0u8; 4096];
    dev.read_block(sb.root_slot_b, &mut mb).unwrap();
    let gen = |b: &[u8]| u64::from_le_bytes(b[8..16].try_into().unwrap());
    let bmp_expect = kos::store::persist::bitmap_blocks_for(sb.obj_pool_blocks as usize);
    println!(
        "[{tag}] gen_a={} gen_b={} obj_pool_blocks={} rec_pool_blocks={} hint={:?} bmp_expect={}",
        gen(&ma), gen(&mb), sb.obj_pool_blocks, sb.rec_pool_blocks, hint, bmp_expect
    );
    if let Some(h) = hint {
        // 位图头几块与索引链首块的 magic。
        let mut buf = [0u8; 4096];
        for i in 0..h.bitmap_blocks.min(3) {
            dev.read_block(sb.rec_pool_start + i as u64, &mut buf).unwrap();
            let crc_ok = kos::store::persist::bitmap_block_parse(&buf).is_some();
            println!("  bmp[{i}] magic={:02x?} parse_ok={crc}", &buf[0..4], crc = crc_ok);
        }
        let mut buf2 = [0u8; 4096];
        dev.read_block(h.index_head, &mut buf2).unwrap();
        println!("  idx_head={} magic={:02x?}", h.index_head, &buf2[0..4]);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let gib: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2048);
    let path = std::env::temp_dir().join("kos-dbg2.img");
    let _ = std::fs::remove_file(&path);
    let blocks = gib * 1024 * 1024 * 1024 / 4096;
    println!("卷 {gib}GiB = {blocks} 块");
    let dev = FileBlockDevice::open(&path, Some(blocks)).unwrap();
    let mut s = KosStore::mkfs(dev).unwrap();
    s.mkdir("/data", 0, perm::DIR_DEFAULT).unwrap();
    let big: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    s.write("/data/blob", &big, 0, perm::DEFAULT).unwrap();
    s.write_mutable("/data/journal", b"j0\n", 0, perm::DEFAULT).unwrap();
    for i in 0..100 {
        let line = format!("entry-{i:06}\n");
        s.append("/data/journal", line.as_bytes(), 0).unwrap();
        if i == 0 || i == 99 {
            println!("  append {i} done, gen={}", s.generation());
        }
    }
    println!("s 写入完成 gen={} objects={} free={}", s.generation(), s.object_count(), s.free_blocks());
    let d = s.take_device();
    dump_stage(&d, "s 落盘后");
    let mut s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    println!("s2 fast={}", s2.was_fast_mounted());
    s2.write("/data/after-remount", b"ok", 0, perm::DEFAULT).unwrap();
    let d2 = s2.take_device();
    dump_stage(&d2, "s2 写入后");
    let mut s3 = KosStore::open(d2, VerifyLevel::Full).unwrap();
    println!("s3 fast={}", s3.was_fast_mounted());
    let _ = std::fs::remove_file(&path);
}
