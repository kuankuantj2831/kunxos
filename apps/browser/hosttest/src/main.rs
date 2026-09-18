//! TLS 模块宿主机端到端测试。
//!
//! 与来宾侧共用 apps/browser/src/tls/ 同一份源码，传输层换成 std
//! TcpStream，对真实站点做完整 TLS 1.2 握手 + 证书链验证 + 取页。
//!
//! 用法：tls-hosttest [host] [port]   （默认 www.baidu.com 443）

extern crate alloc;
extern crate kunxsec as kxsec;

#[path = "../../src/tls/mod.rs"]
mod tls;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct StdTr {
    s: TcpStream,
}

impl tls::Transport for StdTr {
    fn send_all(&mut self, data: &[u8]) -> Result<(), &'static str> {
        self.s.write_all(data).map_err(|_| "发送失败")
    }
    fn recv_some(&mut self, buf: &mut [u8]) -> Result<usize, &'static str> {
        match self.s.read(buf) {
            Ok(0) => Ok(0),
            Ok(n) => Ok(n),
            Err(e) => {
                eprintln!("recv 错误: {:?} ({})", e.kind(), e);
                Err("接收失败")
            }
        }
    }
}

fn main() {
    // 诊断：打印每张证书的 basicConstraints / 有效期解析结果
    let args0: Vec<String> = std::env::args().collect();
    if args0.get(1).map(|s| s.as_str()) == Some("chaindump") {
        tls::x509::VERIFY_DISABLED.store(true, std::sync::atomic::Ordering::Relaxed);
        let host = args0.get(2).cloned().unwrap_or("www.baidu.com".into());
        let addr = format!("{}:443", host);
        let stream = TcpStream::connect(&addr).expect("TCP 连接失败");
        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let mut rng = |b: &mut [u8]| {
            let mut f = std::fs::File::open("/dev/urandom").unwrap();
            f.read_exact(b).unwrap();
        };
        match tls::TlsConn::connect(StdTr { s: stream }, &host, &mut rng, now) {
            Ok(c) => {
                for (i, (der, _)) in c.certs().iter().enumerate() {
                    match tls::x509::parse_cert(der) {
                        Some(cc) => println!(
                            "cert[{}] len={} is_ca={} nb={} na={} cn={:?} san_n={}",
                            i, der.len(), cc.is_ca, cc.not_before, cc.not_after, cc.cn, cc.san.len()
                        ),
                        None => println!("cert[{}] len={} 解析失败", i, der.len()),
                    }

                }
            }
            Err(e) => {
                println!("connect err={}", e);
                std::process::exit(2);
            }
        }
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("selftest") {
        rsa_selftest();
        return;
    }
    gcm_selftest();
    hmac_selftest();
    {
        use std::fs::File;
        let mut f = File::open("/dev/urandom").unwrap();
        let ok = tls::p256::selftest(&mut |b: &mut [u8]| {
            std::io::Read::read_exact(&mut f, b).unwrap();
        });
        println!("P256 selftest: {}", ok);
        if !ok {
            std::process::exit(1);
        }
    }
    let host = args.get(1).cloned().unwrap_or_else(|| "www.baidu.com".into());
    let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(443);
    let path = args.get(3).cloned().unwrap_or_else(|| "/".into());

    if std::env::var("TLS_SKIP_VERIFY").is_ok() {
        tls::x509::VERIFY_DISABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let addr = format!("{}:{}", host, port);
    let stream = TcpStream::connect(&addr).expect("TCP 连接失败");
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let mut rng = |b: &mut [u8]| {
        let mut f = std::fs::File::open("/dev/urandom").unwrap();
        f.read_exact(b).unwrap();
    };

    let t0 = Instant::now();
    let mut conn = match tls::TlsConn::connect(StdTr { s: stream }, &host, &mut rng, now) {
        Ok(c) => {
            println!("TLS 握手成功，耗时 {:?}", t0.elapsed());
            // 打印证书信息
            let certs = c.certs();
            println!("收到 {} 张证书：", certs.len());
            for (i, (cert_der, issuer_der)) in certs.iter().enumerate() {
                println!("  [{}] 主题长度 {}，签发者长度 {}",
                    i, cert_der.len(), issuer_der.len());
            }
            c
        }
        Err(e) => {
            eprintln!("TLS 握手失败：{}", e);
            std::process::exit(1);
        }
    };
    println!("TLS 握手成功，耗时 {:?}", t0.elapsed());

    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: kunxos-hosttest/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path, host
    );
    conn.send(req.as_bytes()).expect("发送失败");
    conn.close();

    let mut resp = Vec::new();
    let mut buf = [0u8; 16384];
    let t1 = Instant::now();
    loop {
        match conn.recv(&mut buf) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(e) => {
                eprintln!("接收错误：{}", e);
                break;
            }
        }
    }
    println!("接收 {} 字节，耗时 {:?}", resp.len(), t1.elapsed());

    let head_end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(0);
    let head = String::from_utf8_lossy(&resp[..head_end.min(resp.len())]);
    for line in head.lines().take(6) {
        println!("| {}", line);
    }
    let body = &resp[head_end.min(resp.len())..];
    let text = String::from_utf8_lossy(&body[..body.len().min(400)]);
    println!("正文开头：{}", text.replace('\n', " ").replace('\r', ""));
    if resp.is_empty() {
        std::process::exit(2);
    }
}

/// RSA 验签自测：用 openssl 生成的签名验证 rsa.rs 实现。
/// 用法：先生成 openssl genrsa/dgst -sign 的 key/sig/pub，传路径。
fn rsa_selftest() {
    let key_der = std::fs::read("/tmp/rsa_test_pub.pem").unwrap();
    // 解析 PEM → DER（PUBLIC KEY）
    let pem = String::from_utf8_lossy(&key_der);
    let b64: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    let der = b64_decode(&b64);
    parse_and_verify(&der);
}

fn parse_and_verify(spki_der: &[u8]) {
    // SubjectPublicKeyInfo ::= SEQUENCE { algorithm, BIT STRING }
    // 简易 DER 游走
    let (t, s, e, _) = tlv(spki_der, 0);
    assert_eq!(t, 0x30);
    let (t2, s2, e2, next) = tlv(spki_der, s);
    assert_eq!(t2, 0x30);
    let _ = (s2, e2);
    let (t3, s3, e3, _) = tlv(spki_der, next);
    assert_eq!(t3, 0x03);
    assert_eq!(spki_der[s3], 0);
    let inner = &spki_der[s3 + 1..e3];
    let (t4, s4, e4, next4) = tlv(inner, 0);
    assert_eq!(t4, 0x30);
    let _ = (next4, s4, e4);
    let (t5, s5, e5, next5) = tlv(inner, s4);
    assert_eq!(t5, 0x02);
    let mut modulus = inner[s5..e5].to_vec();
    let (t6, s6, e6, _) = tlv(inner, next5);
    assert_eq!(t6, 0x02);
    let mut exp = inner[s6..e6].to_vec();
    while modulus.len() > 1 && modulus[0] == 0 {
        modulus.remove(0);
    }
    while exp.len() > 1 && exp[0] == 0 {
        exp.remove(0);
    }
    println!("modulus {} 字节，头 4：{:02X?} 尾 2：{:02X?}", modulus.len(), &modulus[..4], &modulus[modulus.len()-2..]);
    // from_be 隔离测试：0x80 00..00 01（2048 位，顶位 1）
    let mut test_mod = vec![0u8; 256];
    test_mod[0] = 0x80;
    test_mod[255] = 0x01;
    let k = tls::rsa::RsaPub::parse(&test_mod, &exp).expect("parse test mod");
    println!("isolated: {:?}", k.debug_n());
    println!("exp {} 字节：{:02X?}", exp.len(), &exp);
    let pubk = tls::rsa::RsaPub::parse(&modulus, &exp).expect("parse key");
    println!("debug_n: {:?}", pubk.debug_n());
    let msg_hash = kxsec::sha256::sha256(b"hello kunxos");
    let sig = std::fs::read("/tmp/rsa_test.sig").unwrap();
    // 已知答案：sig=1 → em=1；sig=n−1（e 奇数）→ em=n−1
    let mut sig_one = vec![0u8; 256];
    sig_one[255] = 1;
    let em_one = pubk.modexp_em(&sig_one).unwrap();
    println!("em(1) 尾 4：{:02X?}（期望 [0,0,0,1]）", &em_one[em_one.len()-4..]);
    let mut sig_n1 = modulus.clone();
    // n−1：从最低字节借位减 1
    for i in (0..sig_n1.len()).rev() {
        if sig_n1[i] > 0 { sig_n1[i] -= 1; break; }
        sig_n1[i] = 0xFF;
    }
    let em_n1 = pubk.modexp_em(&sig_n1).unwrap();
    println!("em(n−1) 尾 4：{:02X?}（期望与 n−1 尾一致 {:02X?}）", &em_n1[em_n1.len()-4..], &sig_n1[sig_n1.len()-4..]);
    let two = { let mut v = vec![0u8; 256]; v[255] = 2; v };
    println!("mul_mont(2,2) = {:02X?}", pubk.debug_mul_mont(&two, &two).unwrap()[..24].to_vec());
    let (rr, r2) = pubk.debug_rr2();
    println!("r_mod_n  头8：{:02X?} 尾4：{:02X?}", &rr[..8], &rr[rr.len()-4..]);
    println!("r2_mod_n 头8：{:02X?} 尾4：{:02X?}", &r2[..8], &r2[r2.len()-4..]);
    let em = pubk.modexp_em(&sig).expect("modexp");
    println!("em 长度 {} 头 16 字节：{:02X?}", em.len(), &em[..16.min(em.len())]);
    println!("em 尾 40 字节：{:02X?}", &em[em.len() - 40.min(em.len())..]);
    let ok = pubk.verify_pkcs1_sha256(&msg_hash, &sig);
    println!("RSA selftest: {}", if ok { "PASS" } else { "FAIL" });
    let bad_hash = kxsec::sha256::sha256(b"hello kunxox");
    let ok2 = pubk.verify_pkcs1_sha256(&bad_hash, &sig);
    println!("RSA tamper-reject: {}", if !ok2 { "PASS" } else { "FAIL" });
}

fn tlv(b: &[u8], pos: usize) -> (u8, usize, usize, usize) {
    let tag = b[pos];
    let first = b[pos + 1];
    let (len, hdr) = if first & 0x80 == 0 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7F) as usize;
        let mut v = 0usize;
        for i in 0..n {
            v = (v << 8) | b[pos + 2 + i] as usize;
        }
        (v, 2 + n)
    };
    (tag, pos + hdr, pos + hdr + len, pos + hdr + len)
}

fn b64_decode(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=' && c != b'\r' && c != b'\n').collect();
    for chunk in bytes.chunks(4) {
        let v: Vec<usize> = chunk.iter().map(|&c| T.iter().position(|&t| t == c).unwrap()).collect();
        let n = (v[0] << 18) | (v[1] << 12) | (if v.len() > 2 { v[2] << 6 } else { 0 }) | if v.len() > 3 { v[3] } else { 0 };
        out.push((n >> 16) as u8);
        if v.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if v.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

/// NIST GCM 测试向量（Test Case 4，AES-128，IV=12 字节）。
fn gcm_selftest() {
    let key = [
        0xfe, 0xff, 0xe9, 0x92, 0x86, 0x65, 0x73, 0x1c, 0x6d, 0x6a, 0x8f, 0x94, 0x67, 0x30, 0x83,
        0x08,
    ];
    let iv = [0xca, 0xfe, 0xba, 0xbe, 0xfa, 0xce, 0xdb, 0xad, 0xde, 0xca, 0xf8, 0x88];
    let pt_bytes: [u8; 60] = [
        0xd9, 0x31, 0x32, 0x25, 0xf8, 0x84, 0x06, 0xe5, 0xa5, 0x59, 0x09, 0xc5, 0xaf, 0xf5, 0x26,
        0x9a, 0x86, 0xa7, 0xa9, 0x53, 0x15, 0x34, 0xf7, 0xda, 0x2e, 0x4c, 0x30, 0x3d, 0x8a, 0x31,
        0x8a, 0x72, 0x1c, 0x3c, 0x0c, 0x95, 0x95, 0x68, 0x09, 0x53, 0x2f, 0xcf, 0x0e, 0x24, 0x49,
        0xa6, 0xb5, 0x25, 0xb1, 0x6a, 0xed, 0xf5, 0xaa, 0x0d, 0xe6, 0x57, 0xba, 0x63, 0x7b, 0x39,
    ];
    let aad = [
        0xfe, 0xed, 0xfa, 0xce, 0xde, 0xad, 0xbe, 0xef, 0xfe, 0xed, 0xfa, 0xce, 0xde, 0xad, 0xbe,
        0xef, 0xab, 0xad, 0xda, 0xd2,
    ];
    let expect_ct: [u8; 60] = [
        0x42, 0x83, 0x1e, 0xc2, 0x21, 0x77, 0x74, 0x24, 0x4b, 0x72, 0x21, 0xb7, 0x84, 0xd0, 0xd4,
        0x9c, 0xe3, 0xaa, 0x21, 0x2f, 0x2c, 0x02, 0xa4, 0xe0, 0x35, 0xc1, 0x7e, 0x23, 0x29, 0xac,
        0xa1, 0x2e, 0x21, 0xd5, 0x14, 0xb2, 0x54, 0x66, 0x93, 0x1c, 0x7d, 0x8f, 0x6a, 0x5a, 0xac,
        0x84, 0xaa, 0x05, 0x1b, 0xa3, 0x0b, 0x39, 0x6a, 0x0a, 0xac, 0x97, 0x3d, 0x58, 0xe0, 0x91,
    ];
    let expect_tag = [
        0x5b, 0xc9, 0x4f, 0xbc, 0x32, 0x21, 0xa5, 0xdb, 0x94, 0xfa, 0xe9, 0x5a, 0xe7, 0x12, 0x1a,
        0x47,
    ];
    let gcm = tls::aesgcm::Gcm::new(&key);
    let mut buf = pt_bytes;
    let tag = gcm.seal(&iv, &aad, &mut buf);
    println!(
        "GCM selftest: ct={} tag={}",
        buf == expect_ct,
        tag == expect_tag
    );
    let ok = gcm.open(&iv, &aad, &mut buf, &tag);
    println!("GCM open: {} pt={}", ok, buf == pt_bytes);
}

/// RFC 4231 HMAC-SHA256 测试向量 TC1/TC2。
fn hmac_selftest() {
    let t1 = tls::hmac_sha256(&[0x0b_u8; 20], b"Hi There");
    let e1 = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
    let t2 = tls::hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    let e2 = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
    println!(
        "HMAC selftest: tc1={} tc2={}",
        t1.iter().map(|b| format!("{:02x}", b)).collect::<String>() == e1,
        t2.iter().map(|b| format!("{:02x}", b)).collect::<String>() == e2,
    );
}
