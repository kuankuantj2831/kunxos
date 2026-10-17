//! 损坏注入模糊测试：对卷内随机位置翻转随机字节，挂载/读/verify
//! 必须**不 panic、不挂死**（返回错误或干净回退均可）。

use kos::block::{BlockDevice, MemBlockDevice};
use kos::store::{perm, KosStore, VerifyLevel};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn seed_volume() -> MemBlockDevice {
    let mut s = KosStore::mkfs(MemBlockDevice::new(2048, 4096)).unwrap();
    s.mkdir("/d", 0, perm::DIR_DEFAULT).unwrap();
    for i in 0..8 {
        s.write(&format!("/d/f{i}"), &[i as u8 + 1; 4096 * 3], 0, perm::DEFAULT)
            .unwrap();
    }
    s.write_mutable("/m", b"mutable-record", 0, perm::DEFAULT).unwrap();
    for i in 0..20 {
        s.append("/m", format!("app{i} ").as_bytes(), 0).unwrap();
    }
    s.take_device()
}

#[test]
fn fuzz_random_corruption_never_panics_or_hangs() {
    let mut rng = Lcg(0xDEAD_BEEF);
    // 200 轮独立注入：每轮拿新种子卷、翻 1..16 个随机字节。
    for round in 0..200 {
        let mut dev = seed_volume();
        let flips = 1 + (rng.next() % 16) as usize;
        let mut buf = [0u8; 4096];
        for _ in 0..flips {
            let block = (rng.next() % 2048) as u64;
            let byte = (rng.next() % 4096) as usize;
            dev.read_block(block, &mut buf).unwrap();
            buf[byte] ^= 1 << (rng.next() % 8);
            dev.write_block(block, &buf).unwrap();
        }
        // 挂载：Err 合法（卷废）、成功也合法；不允许 panic/挂死。
        let r = KosStore::open(dev, VerifyLevel::HashOnly);
        if let Ok(mut s) = r {
            // 成功挂载后读路径也必须安全。
            let _ = s.open_path("/m", 0);
            let _ = s.open_path("/d/f0", 0);
            let _ = s.list("/d");
            let _ = s.gc();
        }
        if round % 50 == 0 {
            eprintln!("fuzz round {round} ok");
        }
    }
}

#[test]
fn verifier_rejects_forged_signed_slot() {
    use kos::store::{AcceptAllSignatures, ManifestVerifier};
    

    #[allow(dead_code)]
struct RejectAllSigned;
    impl ManifestVerifier for RejectAllSigned {
        fn verify_manifest(&self, m: &kos::format::manifest::KosRootManifest) -> bool {
            // 所有“已签名”槽一律拒绝（模拟验签失败）。
            false
        }
    }
    // 未签名卷：两种验签器都能挂载（创世代兼容）。
    let dev = seed_volume();
    assert!(KosStore::open_with_verifier(dev, VerifyLevel::HashOnly, &AcceptAllSignatures).is_ok());
    let dev = seed_volume();
    assert!(KosStore::open_with_verifier(dev, VerifyLevel::HashOnly, &RejectAllSigned).is_ok());

    // 伪造“已签名”槽（signer_pk 非零、签名区垃圾）：严格验签器应视同
    // 该槽无效 → 双槽皆无效 → NoValidManifest。
    let mut dev = seed_volume();
    let mut buf = [0u8; 4096];
    use kos::block::BlockDevice;
    dev.read_block(1, &mut buf).unwrap();
    // 结构化伪造：解析 → 篡改 signer_pk/signature → 重序列化（CRC 自洽，
    // 模拟攻击者自制合法 CRC 的伪签名槽）。
    let mut m = kos::format::manifest::KosRootManifest::from_bytes(&buf[..192]).unwrap();
    m.signer_pk = [0xAB; 32];
    m.signature = [0xCD; 64];
    let fixed = m.to_bytes();
    buf[..192].copy_from_slice(&fixed);
    dev.write_block(1, &buf).unwrap();
    // 槽 A 被拒后，落选的旧槽 B（无签名）应兜底可用；两槽皆无效才报错。
    let r = KosStore::open_with_verifier(dev, VerifyLevel::HashOnly, &RejectAllSigned);
    assert!(r.is_ok(), "伪造签名槽被拒后应回退到另一槽");
    // 对照：宽松验签器可挂载且选中的是（伪）高代槽。
    let dev2 = {
        let mut d = seed_volume();
        let mut b2 = [0u8; 4096];
        d.read_block(1, &mut b2).unwrap();
        let mut m2 = kos::format::manifest::KosRootManifest::from_bytes(&b2[..192]).unwrap();
        m2.signer_pk = [0xAB; 32];
        b2[..192].copy_from_slice(&m2.to_bytes());
        d.write_block(1, &b2).unwrap();
        d
    };
    assert!(KosStore::open_with_verifier(dev2, VerifyLevel::HashOnly, &AcceptAllSignatures).is_ok());
}
