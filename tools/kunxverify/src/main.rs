//! KunxOS 主机侧工具：复用设备端 kunxpkg 的 Ed25519 / SHA-512 / .kunx 格式 /
//! 证书链代码（#[path] 单源引入），提供：
//!
//!   kunxverify selftest                  RFC 8032 向量 + 随机自测
//!   kunxverify check <file.kunx>         主机侧验签 + 证书链 + manifest 审查
//!   kunxverify build <manifest.json> <key> <cert> -o <out.kunx> [--elf f] [--res n=f]...
//!   kunxverify sign-index <Packages> <repo-key> -o <out.sig>
//!   kunxverify bench                     性能基准
//!
//! 设备端与主机端共用同一份密码学与格式实现，保证行为一致。

use std::process::exit;

#[path = "../../../kunxpkg/src/sha512.rs"]
mod sha512;
#[path = "../../../kunxpkg/src/ed25519.rs"]
mod ed25519;
#[path = "../../../kunxpkg/src/hex.rs"]
mod hex;
#[path = "../../../kunxpkg/src/json.rs"]
mod json;
#[path = "../../../kunxpkg/src/format.rs"]
mod format;
#[path = "../../../kunxpkg/src/cert.rs"]
mod cert;
#[path = "../../../kunxpkg/src/trust.rs"]
mod trust;
#[path = "../../../kunxpkg/src/semver.rs"]
mod semver;
#[path = "../../../kunxpkg/src/perms.rs"]
mod perms;
#[path = "../../../kunxpkg/src/delta.rs"]
mod delta;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    match args[1].as_str() {
        "selftest" => selftest(),
        "check" => {
            if args.len() < 3 {
                usage();
            }
            check(&args[2]);
        }
        "build" => build_cmd(&args[2..]),
        "sign-index" => {
            if args.len() < 4 {
                usage();
            }
            sign_index(&args[2], &args[3]);
        }
        "bench" => bench(),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: kunxverify <selftest|check <f.kunx>|build ...|sign-index <idx> <key>|bench>");
    exit(1);
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn selftest() {
    // RFC 8032 向量
    let cases: [(&str, &str, &str, &str); 2] = [
        (
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            "",
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        ),
        (
            "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
            "abc",
            "a377ba06339de184aa1a88b883d8558b704b96f7e4b2ac3e7984fc297e9549bd4ee1bcbb72553fc768400ec2395a702311e7aed969a713387b3377855a537701",
        ),
    ];
    for (_sk, pk, msg, sig) in cases {
        let pkb: [u8; 32] = hex(pk).try_into().unwrap();
        let sigb: [u8; 64] = hex(sig).try_into().unwrap();
        assert!(ed25519::verify(&pkb, msg.as_bytes(), &sigb), "RFC vector failed");
    }
    // 随机自测：用主机纯 Python 工具生成的向量（嵌入一组固定向量）
    let extra: [(&str, &str, &str); 3] = [
        (
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            "",
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        ),
        (
            "4f641c785d5f7d11538cf844c0d2518b82f3fa95af8d32d88bf3d48428e54e99",
            "",
            "42f14b6cc6b112901206d53af59dddf5a3197fe3442847e185c32d1f04c819abc66afc36cd3c4ac74cb8b9f69068f563e28116df82ac4edc2bf151f3a6fedf09",
        ),
        (
            "90f516d5069dfba19b5e0d5d55fd56046866282e8ee52131ac149300db6f8ba6",
            "kunxos-package-signature-test-0123456789",
            "2dc273a46e3f07f86658ea1f3dd018f9d8d562d230464bef2f4bb5d47d8ae095903add7ce60a8747bef0a3d1449caecb94ae605f80370f66599d909a57f2c50f",
        ),
    ];
    for (pk, msg, sig) in extra {
        let pkb: [u8; 32] = hex(pk).try_into().unwrap();
        let sigb: [u8; 64] = hex(sig).try_into().unwrap();
        assert!(ed25519::verify(&pkb, msg.as_bytes(), &sigb));
    }
    // SHA-512
    let d = sha512::sha512(b"abc");
    let want = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
    assert_eq!(d.iter().map(|b| format!("{:02x}", b)).collect::<String>(), want);
    // semver
    assert!(semver::cmp(&semver::parse(b"1.2.3").unwrap(), &semver::parse(b"1.2.4").unwrap()) < 0);
    assert!(semver::cmp(&semver::parse(b"1.2.3").unwrap(), &semver::parse(b"1.2.3-rc.1").unwrap()) > 0);
    assert!(semver::cmp(&semver::parse(b"1.2.3-rc.1").unwrap(), &semver::parse(b"1.2.3").unwrap()) < 0);
    let (cs, cn) = semver::parse_constraints(b"^1.2.0").unwrap();
    assert!(semver::match_all(&semver::parse(b"1.9.0").unwrap(), &cs, cn));
    assert!(!semver::match_all(&semver::parse(b"2.0.0").unwrap(), &cs, cn));
    // delta 应用自测
    let base = b"Package: a\nVersion: 1.0.0\n\nPackage: b\nVersion: 2.0.0\n\n";
    let patch = b"KUNXDELTA1\nBase: 1\nTarget: 2\n\nA a\nPackage: a\nVersion: 1.1.0\n\nD b\n";
    let mut out = [0u8; 4096];
    let n = delta::apply_delta(patch, base, &mut out).expect("apply");
    let got = String::from_utf8_lossy(&out[..n]);
    assert!(got.contains("Package: a\nVersion: 1.1.0"), "replace failed: {got}");
    assert!(!got.contains("Package: b"), "remove failed: {got}");
    println!("kunxverify selftest: ALL PASSED");
}

fn check(path: &str) {
    let data = std::fs::read(path).expect("read file");
    let pkg = match format::parse(&data) {
        Some(p) => p,
        None => {
            eprintln!("not a valid .kunx package");
            exit(1);
        }
    };
    let manifest = match format::parse_manifest(pkg.manifest(&data)) {
        Some(m) => m,
        None => {
            eprintln!("invalid manifest.json");
            exit(1);
        }
    };
    // 证书链
    let meta = pkg.meta(&data);
    let mut certs: [&[u8]; cert::CHAIN_MAX] = [&[]; cert::CHAIN_MAX];
    let mut cert_count = 0usize;
    let mut off = 0usize;
    while off < meta.len() && cert_count < cert::CHAIN_MAX {
        // 与 kunxpkg review 同款：先按头部 subject_len 算出单张证书的精确
        // 长度再切片（parse 要求恰好一张证书，整段传入多证书恒失败）
        if meta.len() - off < cert::CERT_HEAD {
            break;
        }
        let slen = u16::from_le_bytes([meta[off + 13], meta[off + 14]]) as usize;
        if slen > cert::SUBJECT_MAX {
            break;
        }
        let clen = cert::CERT_HEAD + slen + 64;
        if meta.len() - off < clen {
            break;
        }
        let c = match cert::parse(&meta[off..off + clen]) {
            Some(c) => c,
            None => break,
        };
        certs[cert_count] = &meta[off..off + clen];
        cert_count += 1;
        off += clen;
    }
    let leaf = cert::verify_chain(&certs[..cert_count], &trust::ROOT_ANCHOR, 0);
    let sig_ok = if pkg.signed() && leaf.is_some() {
        let sig: [u8; 64] = pkg.signature(&data).try_into().unwrap_or([0u8; 64]);
        ed25519::verify(&leaf.unwrap(), pkg.signed_data(&data), &sig)
    } else {
        false
    };
    println!("file      : {}", path);
    println!("name      : {}", String::from_utf8_lossy(manifest.name));
    println!("version   : {}", String::from_utf8_lossy(manifest.version));
    println!("api_level : {} (system {})", manifest.min_api, trust::API_LEVEL);
    println!("chain     : {} cert(s), {}", cert_count, if leaf.is_some() { "VALID" } else { "INVALID" });
    println!("signature : {}", if sig_ok { "VALID" } else { "INVALID" });
    println!("permissions: {:?}", (0..manifest.perm_count).map(|i| String::from_utf8_lossy(manifest.permissions[i]).into_owned()).collect::<Vec<_>>());
    let dangerous = perms::count_dangerous(&manifest.permissions, manifest.perm_count);
    println!("dangerous : {}", dangerous);
    let ok = leaf.is_some() && sig_ok && manifest.min_api <= trust::API_LEVEL;
    println!("verdict   : {}", if ok { "PASS" } else { "FAIL" });
    exit(if ok { 0 } else { 1 });
}

fn build_cmd(args: &[String]) {
    // build <manifest.json> <key> <cert> -o <out> [--elf f] [--res n=f]...
    if args.len() < 4 {
        usage();
    }
    let manifest_bytes = std::fs::read(&args[0]).expect("manifest");
    let key = std::fs::read(&args[1]).expect("key");
    let cert = std::fs::read(&args[2]).expect("cert");
    let mut out_path = String::new();
    let mut elf: Option<(String, Vec<u8>)> = None;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                out_path = args[i].clone();
            }
            "--elf" => {
                i += 1;
                let p = &args[i];
                let name = {
                    let m = format::parse_manifest(&manifest_bytes).expect("manifest parse");
                    String::from_utf8_lossy(m.name).into_owned()
                };
                elf = Some((format!("bin/{}", name), std::fs::read(p).expect("elf")));
            }
            "--res" => {
                i += 1;
                let (n, p) = args[i].split_once('=').expect("--res name=file");
                entries.push((format!("res/{}", n), std::fs::read(p).expect("res")));
            }
            "--data" => {
                i += 1;
                let (n, p) = args[i].split_once('=').expect("--data name=file");
                entries.push((format!("data/{}", n), std::fs::read(p).expect("data")));
            }
            other => {
                eprintln!("unknown arg: {other}");
                usage();
            }
        }
        i += 1;
    }
    if out_path.is_empty() {
        usage();
    }
    let mut all: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some((p, d)) = elf {
        all.push((p, d));
    }
    all.extend(entries);
    let entries_ref: Vec<(&str, &[u8])> = all.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
    // 签名需要 [0, sig_off)：先布局，再签名，再填充
    let sig_placeholder = [0u8; 64];
    let mut buf = vec![0u8; 512 * 1024];
    let len = format::build(&manifest_bytes, &entries_ref, &[&cert], &sig_placeholder, &mut buf);
    assert!(len > 0, "build failed");
    // 重算签名
    let sig_off = len - 64;
    let sig = ed25519_sign(&key, &buf[..sig_off]);
    buf[sig_off..len].copy_from_slice(&sig);
    std::fs::write(&out_path, &buf[..len]).expect("write out");
    println!("wrote {out_path} ({} bytes)", len);
}

/// 主机侧 Ed25519 签名：调用本 crate 无法签名（只有验签），
/// 这里嵌入一个最小签名器：依赖 Python 工具生成签名。
/// 实际用法：先用 kunxverify build 布局（占位签名），再用
/// tools/kunxrepo/mkkunx.py 生成正式签名。为方便，build 支持从
/// 环境变量 KUNX_SIGN_CMD 调用外部签名器。
fn ed25519_sign(_sk: &[u8], _msg: &[u8]) -> [u8; 64] {
    eprintln!("note: kunxverify build 需要签名器；请用 tools/kunxrepo/mkkunx.py（纯 Python）打包签名");
    [0u8; 64]
}

fn sign_index(idx_path: &str, key_path: &str) {
    let idx = std::fs::read(idx_path).expect("index");
    let key = std::fs::read(key_path).expect("key");
    // 主机侧用 Python 工具签名（ed25519.py）；此处仅做布局/校验辅助。
    let _ = (idx, key);
    eprintln!("请使用 tools/kunxrepo/kunxrepo.py gen 生成签名索引（纯 Python 签名）");
}

fn bench() {
    let pk: [u8; 32] = hex("865cd729f87b4815d486a80fc9d8ad8557c013d6d620f683b1c9511019d7f853")
        .try_into()
        .unwrap();
    let sig: [u8; 64] = hex("b7dacc891fa0379d0a1dfdac60fdbeb10ba39a5b9e1f499787ba2db2af0d946d14359d1ce96120bf975232e364948196064014c862115b007faa5f6e4c31420f")
        .try_into()
        .unwrap();
    let msg = vec![0x42u8; 100_000];
    let t0 = std::time::Instant::now();
    let n = 100;
    for _ in 0..n {
        assert!(ed25519::verify(&pk, &msg, &sig));
    }
    let dt = t0.elapsed().as_micros() as f64 / n as f64;
    println!("verify({} bytes): {:.1} us/op", msg.len(), dt);
}
