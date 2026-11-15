//! Replacement algorithms must preserve existing digests and disk bytes.
use kos::block::{BlockDevice, MemBlockDevice};
use kos::hash::{Blake3Hasher, Hasher, Sha256Hasher};
use kos::store::{KosStore, VerifyLevel};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn upstream_digest_fixtures_and_streaming() {
    for line in include_str!("data/hash-vectors.txt").trim_start_matches('\u{feff}').lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let len: usize = fields[0].parse().unwrap();
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        assert_eq!(hex(&Blake3Hasher.hash32(&data)), fields[1], "BLAKE3 len={len}");
        assert_eq!(hex(&Sha256Hasher.hash32(&data)), fields[2], "SHA256 len={len}");
        assert_eq!(format!("{:08x}", kunxsec::crc32::crc32(&data)), fields[3], "CRC len={len}");
        for size in [1, 7, 63, 64, 65, 1024] {
            let mut sha = kunxsec::sha256::Sha256::new();
            let mut crc = kunxsec::crc32::Crc32::new();
            for part in data.chunks(size) {
                sha.update(part);
                crc.update(part);
            }
            assert_eq!(hex(&sha.finalize()), fields[2], "stream SHA len={len} size={size}");
            assert_eq!(format!("{:08x}", crc.finalize()), fields[3], "stream CRC len={len} size={size}");
        }
    }
}

fn legacy_device() -> MemBlockDevice {
    let mut dev = MemBlockDevice::new(256, 4096);
    for record in include_bytes!("data/legacy-blocks.bin").chunks_exact(4104) {
        let lba = u64::from_le_bytes(record[..8].try_into().unwrap());
        dev.write_block(lba, &record[8..]).unwrap();
    }
    dev
}

#[test]
fn legacy_disk_mount_read_update_and_remount() {
    let data: Vec<u8> = (0..8193).map(|i| (i % 251) as u8).collect();
    let mut store = KosStore::open(legacy_device(), VerifyLevel::HashOnly).unwrap();
    assert_eq!(store.open_path("/compat/data", 0).unwrap(), data);
    store.write("/compat/new", b"self-developed hashes", 0, 0o644).unwrap();
    let mut store = KosStore::open(store.take_device(), VerifyLevel::HashOnly).unwrap();
    assert_eq!(store.open_path("/compat/data", 0).unwrap(), data);
    assert_eq!(store.open_path("/compat/new", 0).unwrap(), b"self-developed hashes");
}

#[test]
fn new_disk_bytes_match_legacy_implementation() {
    let mut store = KosStore::mkfs(MemBlockDevice::new(256, 4096)).unwrap();
    store.mkdir("/compat", 0, 0o755).unwrap();
    let data: Vec<u8> = (0..8193).map(|i| (i % 251) as u8).collect();
    store.write("/compat/data", &data, 0, 0o644).unwrap();
    let actual = store.take_device();
    let expected = legacy_device();
    let mut a = [0; 4096];
    let mut b = [0; 4096];
    for lba in 0..256 {
        actual.read_block(lba, &mut a).unwrap();
        expected.read_block(lba, &mut b).unwrap();
        assert_eq!(a, b, "disk block {lba}");
    }
}
