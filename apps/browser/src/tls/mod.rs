//! 最小 TLS 1.2 客户端（浏览器 HTTPS 支持）。
//!
//! 密码套件唯一：`TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256` (0xC02F) ——
//! 即 www.baidu.com 等 TLS 1.2 站点实际协商的主流套件。证书链验证见
//! x509.rs（RSA + SHA-256 + 内嵌根）。一次性握手、不复用会话、无重协商。
//!
//! - 传输抽象 [`Transport`]：来宾侧用 socket syscall，宿主侧用 std TCP
//!   （apps/browser/hosttest 用同一份代码对真实站点做端到端验证）。
//! - 记录层：GCM 记录 = 显式 nonce(8B=序号) ‖ 密文 ‖ tag(16B)；
//!   AAD = seq(8) ‖ type ‖ version ‖ record_len（含 nonce+tag）。
//! - 握手消息重组：记录跨界拼接、一记录多消息均支持。
//! - 告警只识别 close_notify（视作 EOF）。收发均为阻塞语义。

pub mod aesgcm;
pub mod p256;
pub mod roots;
pub mod rsa;
pub mod x509;

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use crate::kxsec::sha256::Sha256;

// ---- 记录类型 ----
const CT_CCS: u8 = 20;
const CT_ALERT: u8 = 21;
const CT_HANDSHAKE: u8 = 22;
const CT_APPDATA: u8 = 23;

// ---- 握手类型 ----
const HS_SERVER_HELLO: u8 = 2;
const HS_CERT: u8 = 11;
const HS_SKE: u8 = 12;
const HS_CKX: u8 = 16;
const HS_HELLO_DONE: u8 = 14;
const HS_FINISHED: u8 = 20;

const CIPHER_ECDHE_RSA_AES128_GCM_SHA256: u16 = 0xC02F;

/// 记录层分片上限（标准 2^14；握手/应用都按此分片）。
const MAX_FRAGMENT: usize = 16384;
/// 接受的记录最大长度（2^14 + 2048）。
const MAX_RECORD: usize = 16640;
const MAX_HS_MSG: usize = 32 * 1024;
const MAX_CHAIN: usize = 8;

/// 诊断日志：来宾侧直接 syscall 写 stderr（fd 2），宿主机测试走 eprintln。
pub fn tls_log(s: &str) {
    #[cfg(target_os = "none")]
    unsafe {
        let mut write = |buf: &[u8]| {
            core::arch::asm!(
                "ecall",
                in("a7") 64usize, // SYS_WRITE
                in("a0") 2usize,  // stderr
                in("a1") buf.as_ptr() as usize,
                in("a2") buf.len(),
                options(nostack)
            );
        };
        write(s.as_bytes());
        write(b"\n");
    }
    #[cfg(not(target_os = "none"))]
    eprintln!("{}", s);
}

/// 传输抽象。
pub trait Transport {
    /// 发送全部数据；Err = 连接错误。
    fn send_all(&mut self, data: &[u8]) -> Result<(), &'static str>;
    /// 接收任意长度（≥1）；Ok(0) = 对端关闭；Err = 错误。
    fn recv_some(&mut self, buf: &mut [u8]) -> Result<usize, &'static str>;
}

/// 随机源。
pub trait Rng {
    fn fill(&mut self, buf: &mut [u8]);
}

impl<F: FnMut(&mut [u8])> Rng for F {
    fn fill(&mut self, buf: &mut [u8]) {
        self(buf);
    }
}

// ==================== HMAC-SHA256 / TLS 1.2 PRF ====================

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&crate::kxsec::sha256::sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5Cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut h = Sha256::new();
    h.update(&ipad);
    h.update(data);
    let inner = h.finalize();
    let mut h2 = Sha256::new();
    h2.update(&opad);
    h2.update(&inner);
    h2.finalize()
}

/// TLS 1.2 P_SHA256。
fn prf(secret: &[u8], label: &[u8], seed: &[u8], out: &mut [u8]) {
    let mut base = Vec::with_capacity(label.len() + seed.len());
    base.extend_from_slice(label);
    base.extend_from_slice(seed);
    let mut a = hmac_sha256(secret, &base); // A(1)
    let mut pos = 0;
    while pos < out.len() {
        let mut t_input = Vec::with_capacity(32 + base.len());
        t_input.extend_from_slice(&a);
        t_input.extend_from_slice(&base);
        let t = hmac_sha256(secret, &t_input);
        let take = (out.len() - pos).min(32);
        out[pos..pos + take].copy_from_slice(&t[..take]);
        pos += take;
        a = hmac_sha256(secret, &a);
    }
}

// ==================== 小工具 ====================

fn u24(b: &[u8], pos: usize) -> usize {
    ((b[pos] as usize) << 16) | ((b[pos + 1] as usize) << 8) | b[pos + 2] as usize
}

fn is_dns_name(host: &str) -> bool {
    !host.is_empty()
        && host.bytes().all(|c| {
            c.is_ascii_alphanumeric() || c == b'-' || c == b'.' || c == b'_'
        })
        && host.bytes().any(|c| c.is_ascii_alphabetic())
}

// ==================== 连接 ====================

pub struct TlsConn<T: Transport> {
    tr: T,
    /// 传输层输入暂存（按需取出记录）。
    inbuf: Vec<u8>,
    /// 握手消息重组缓冲。
    hs_buf: Vec<u8>,
    /// 解密后的应用数据剩余。
    pt_buf: Vec<u8>,
    /// 握手 transcript。
    hs_hash: Sha256,
    client_random: [u8; 32],
    server_random: [u8; 32],
    master_secret: [u8; 48],
    client_gcm: aesgcm::Gcm,
    server_gcm: aesgcm::Gcm,
    client_iv: [u8; 4],
    server_iv: [u8; 4],
    cseq: u64,
    sseq: u64,
    c_enc: bool,
    s_enc: bool,
    /// 对端已发 close_notify（EOF）。
    peer_closed: bool,
    /// 证书链（证书 DER + 签发者 DER）。
    certs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl<T: Transport> TlsConn<T> {
    /// 执行 TLS 1.2 握手。`now`：用于证书有效期的当前 unix 秒。
    pub fn connect(
        mut tr: T,
        host: &str,
        rng: &mut dyn Rng,
        now: i64,
    ) -> Result<TlsConn<T>, &'static str> {
        let mut client_random = [0u8; 32];
        rng.fill(&mut client_random);
        let (ecdhe_priv, ecdhe_pub) = p256::make_keypair(&mut |b: &mut [u8]| rng.fill(b));

        let mut conn = TlsConn {
            tr,
            inbuf: Vec::with_capacity(4096),
            hs_buf: Vec::with_capacity(2048),
            pt_buf: Vec::new(),
            hs_hash: Sha256::new(),
            client_random,
            server_random: [0; 32],
            master_secret: [0; 48],
            client_gcm: aesgcm::Gcm::new(&[0; 16]),
            server_gcm: aesgcm::Gcm::new(&[0; 16]),
            client_iv: [0; 4],
            server_iv: [0; 4],
            cseq: 0,
            sseq: 0,
            c_enc: false,
            s_enc: false,
            peer_closed: false,
            certs: Vec::new(),
        };

        // ---- ClientHello ----
        let ch = build_client_hello(host, &conn.client_random, &ecdhe_pub);
        conn.hs_hash.update(&ch);
        let rec = record(CT_HANDSHAKE, 0x0301, &ch);
        conn.tr.send_all(&rec)?;

        // ---- 服务器握手飞行 ----
        let mut server_point: Option<Vec<u8>> = None;
        let mut ske_params: Option<Vec<u8>> = None;
        let mut ske_sig: Option<Vec<u8>> = None;
        let mut saw_hello = false;
        loop {
            let (ty, body) = conn.read_hs_msg()?;
            match ty {
                HS_SERVER_HELLO => {
                    parse_server_hello(&body, &mut conn.server_random)?;
                    saw_hello = true;
                }
                HS_CERT => {
                    let chain = parse_certificate_list(&body)?;
                    for cert_der in chain.iter() {
                        if let Some(cert) = x509::parse_cert(cert_der) {
                            // 签发者 DER 需要时由 issuer TLV 重建
                            let issuer_der = x509::write_tlv(0x30, &cert.issuer);
                            conn.certs.push((cert_der.clone(), issuer_der));
                        }
                    }
                }
                HS_SKE => {
                    let (pt, params, sig) = parse_server_key_exchange(&body)?;
                    server_point = Some(pt);
                    ske_params = Some(params);
                    ske_sig = Some(sig);
                }
                HS_HELLO_DONE => break,
                _ => {} // HelloRequest 等忽略
            }
        }
        if !saw_hello {
            return Err("服务器未响应 TLS 握手");
        }
        let point = server_point.ok_or("服务器未提供 ECDHE 参数")?;
        let ske_params = ske_params.ok_or("服务器未提供 ECDHE 签名参数")?;
        let ske_sig = ske_sig.ok_or("服务器未提供 ECDHE 签名")?;
        if conn.certs.is_empty() {
            return Err("服务器未送证书");
        }

        // ---- 证书链验证 ----
        // 提取 DER 数据用于验证（conn.certs 存储 (der, issuer_der)）
        let cert_der_data: Vec<Vec<u8>> = conn.certs.iter().map(|(der, _)| der.clone()).collect();
        let leaf_key = match x509::verify_chain(&cert_der_data, host, now) {
            Ok(k) => k,
            Err(e) => {
                tls_log(&format!("证书验证失败：{}", e));
                return Err(e);
            }
        };

        // ---- ServerKeyExchange 签名验证 ----
        let mut sig_input = Vec::with_capacity(64 + ske_params.len());
        sig_input.extend_from_slice(&conn.client_random);
        sig_input.extend_from_slice(&conn.server_random);
        sig_input.extend_from_slice(&ske_params);
        let ske_hash = crate::kxsec::sha256::sha256(&sig_input);
        if !leaf_key.verify_pkcs1_sha256(&ske_hash, &ske_sig) {
            return Err("ServerKeyExchange 签名验证失败");
        }

        // ---- 密钥推导 ----
        let premaster = p256::shared_secret(&ecdhe_priv, &point)?;
        let mut seed = Vec::with_capacity(64);
        seed.extend_from_slice(&conn.client_random);
        seed.extend_from_slice(&conn.server_random);
        prf(&premaster, b"master secret", &seed, &mut conn.master_secret);
        let mut key_block = [0u8; 40];
        let mut kb_seed = Vec::with_capacity(64);
        kb_seed.extend_from_slice(&conn.server_random);
        kb_seed.extend_from_slice(&conn.client_random);
        prf(&conn.master_secret, b"key expansion", &kb_seed, &mut key_block);
        let client_key: [u8; 16] = key_block[..16].try_into().unwrap();
        let server_key: [u8; 16] = key_block[16..32].try_into().unwrap();
        conn.client_iv.copy_from_slice(&key_block[32..36]);
        conn.server_iv.copy_from_slice(&key_block[36..40]);
        conn.client_gcm = aesgcm::Gcm::new(&client_key);
        conn.server_gcm = aesgcm::Gcm::new(&server_key);

        // ---- ClientKeyExchange（明文发送，先于 CCS，纳入握手哈希）----
        let mut ckx = Vec::with_capacity(4 + 1 + 65);
        ckx.push(HS_CKX);
        ckx.extend([0u8, 0, 66]); // u24 长度 = 1 字节点长度 + 65 字节点
        ckx.push(65u8);
        ckx.extend_from_slice(&ecdhe_pub);
        conn.send_record(CT_HANDSHAKE, &ckx)?;
        conn.hs_hash.update(&ckx);

        // ---- 客户端 Finished（CCS 后密文发送）----
        let chash = conn.hs_hash.clone().finalize();
        let mut verify = [0u8; 12];
        prf(&conn.master_secret, b"client finished", &chash, &mut verify);
        let mut fin_msg = Vec::with_capacity(16);
        fin_msg.push(HS_FINISHED);
        fin_msg.extend([0u8, 0, 12]);
        fin_msg.extend_from_slice(&verify);
        // CCS 记录：type + 版本 + 长度(0x0001) + 载荷 0x01
        let ccs = [CT_CCS, 0x03, 0x03, 0x00, 0x01, 0x01];
        conn.tr.send_all(&ccs)?;
        conn.c_enc = true;
        conn.cseq = 0;
        conn.send_record(CT_HANDSHAKE, &fin_msg)?;
        conn.hs_hash.update(&fin_msg);

        // ---- 服务器 Finished ----
        // verify_data 只覆盖到服务器 Finished 之前；read_hs_msg 会把该消息
        // 记入哈希，因此先克隆状态再读。
        let pre_fin = conn.hs_hash.clone();
        let (ty, body) = conn.read_hs_msg()?;
        if ty != HS_FINISHED {
            return Err("服务器握手顺序异常");
        }
        let shash = pre_fin.finalize();
        let mut expect = [0u8; 12];
        prf(&conn.master_secret, b"server finished", &shash, &mut expect);
        if body.len() != 12 || body[..] != expect[..] {
            return Err("服务器 Finished 校验失败");
        }
        Ok(conn)
    }

    // ---- 记录层 ----

    fn fill_at_least(&mut self, n: usize) -> Result<(), &'static str> {
        while self.inbuf.len() < n {
            let mut buf = [0u8; 4096];
            let got = self.tr.recv_some(&mut buf)?;
            if got == 0 {
                return Err("TLS 连接被对端关闭");
            }
            self.inbuf.extend_from_slice(&buf[..got]);
            if self.inbuf.len() > MAX_RECORD + 8192 {
                return Err("TLS 输入缓冲溢出");
            }
        }
        Ok(())
    }

    /// 读取并（按需）解密一条记录。明文记录同样推进序号（新纪元清零）。
    fn read_record(&mut self) -> Result<(u8, Vec<u8>), &'static str> {
        self.fill_at_least(5)?;
        let ty = self.inbuf[0];
        let ver = [self.inbuf[1], self.inbuf[2]];
        let len = u16::from_be_bytes([self.inbuf[3], self.inbuf[4]]) as usize;
        if len > MAX_RECORD {
            return Err("TLS 记录超长");
        }
        self.fill_at_least(5 + len)?;
        let raw: Vec<u8> = self.inbuf.drain(..5 + len).skip(5).collect();
        if !self.s_enc {
            self.sseq += 1;
            return Ok((ty, raw));
        }
        if raw.len() < 8 + 16 {
            return Err("TLS 加密记录过短");
        }
        let mut iv = [0u8; 12];
        iv[..4].copy_from_slice(&self.server_iv);
        iv[4..].copy_from_slice(&raw[..8]);
        let mut aad = Vec::with_capacity(13);
        aad.extend_from_slice(&self.sseq.to_be_bytes());
        aad.push(ty);
        aad.extend_from_slice(&ver);
        // AAD 长度字段 = 明文长度（不含 8 字节显式 nonce 与 16 字节 tag）
        aad.extend_from_slice(&((raw.len() - 24) as u16).to_be_bytes());
        let mut pt = raw[8..raw.len() - 16].to_vec();
        let mut tag = [0u8; 16];
        tag.copy_from_slice(&raw[raw.len() - 16..]);
        if !self.server_gcm.open(&iv, &aad, &mut pt, &tag) {
            return Err("TLS 记录解密失败（完整性校验不符）");
        }
        self.sseq += 1;
        Ok((ty, pt))
    }

    fn send_record(&mut self, ty: u8, payload: &[u8]) -> Result<(), &'static str> {
        for chunk in payload.chunks(MAX_FRAGMENT) {
            let ver: u16 = 0x0303;
            if !self.c_enc {
                let mut rec = Vec::with_capacity(5 + chunk.len());
                rec.push(ty);
                rec.extend_from_slice(&ver.to_be_bytes());
                rec.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
                rec.extend_from_slice(chunk);
                self.tr.send_all(&rec)?;
                self.cseq += 1;
                continue;
            }
            let seq = self.cseq;
            let mut iv = [0u8; 12];
            iv[..4].copy_from_slice(&self.client_iv);
            iv[4..].copy_from_slice(&seq.to_be_bytes());
            let mut aad = Vec::with_capacity(13);
            aad.extend_from_slice(&seq.to_be_bytes());
            aad.push(ty);
            aad.extend_from_slice(&ver.to_be_bytes());
            // AAD 长度字段 = 明文长度（不含 8 字节显式 nonce 与 16 字节 tag）
            aad.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
            let mut pt = chunk.to_vec();
            let tag = self.client_gcm.seal(&iv, &aad, &mut pt);
            let mut rec = Vec::with_capacity(5 + 8 + pt.len() + 16);
            rec.push(ty);
            rec.extend_from_slice(&ver.to_be_bytes());
            rec.extend_from_slice(&((pt.len() + 24) as u16).to_be_bytes());
            rec.extend_from_slice(&seq.to_be_bytes());
            rec.extend_from_slice(&pt);
            rec.extend_from_slice(&tag);
            self.tr.send_all(&rec)?;
            self.cseq += 1;
        }
        Ok(())
    }

    /// 读一条完整握手消息（跨记录重组；顺带处理 CCS/告警）。
    fn read_hs_msg(&mut self) -> Result<(u8, Vec<u8>), &'static str> {
        loop {
            if self.hs_buf.len() >= 4 {
                let ty = self.hs_buf[0];
                let len = u24(&self.hs_buf, 1);
                if len > MAX_HS_MSG {
                    return Err("TLS 握手消息超长");
                }
                if self.hs_buf.len() >= 4 + len {
                    let body = self.hs_buf[4..4 + len].to_vec();
                    self.hs_buf.drain(..4 + len);
                    // RFC 5246 §7.4.1.1：HelloRequest 不属于握手状态机，
                    // 不得计入握手哈希（否则 Finished 校验必失败）。
                    if ty != 0 {
                        self.hs_hash.update(&[ty]);
                        self.hs_hash.update(&[(len >> 16) as u8, (len >> 8) as u8, len as u8]);
                        self.hs_hash.update(&body);
                    }
                    return Ok((ty, body));
                }
            }
            let (ty, body) = self.read_record()?;
            match ty {
                CT_HANDSHAKE => self.hs_buf.extend_from_slice(&body),
                CT_CCS => {
                    self.s_enc = true;
                    self.sseq = 0;
                }
                CT_ALERT => {
                    if body.len() < 2 {
                        // 畸形告警（0/1 字节）：不能索引 body[0]/body[1]——
                        // 在路径攻击者注入空告警记录即可远程 panic 浏览器。
                        return Err("TLS 畸形告警记录");
                    }
                    if body[1] == 0 {
                        return Err("TLS 对端关闭（close_notify）");
                    }
                    {
                        tls_log(&format!("TLS 告警: level={} desc={}", body[0], body[1]));
                        return Err("TLS 收到告警");
                    }
                }
                CT_APPDATA => return Err("TLS 握手期间收到应用数据"),
                _ => {}
            }
        }
    }

    // ---- 应用数据 ----

    /// 发送应用数据（自动分片）。
    pub fn send(&mut self, data: &[u8]) -> Result<(), &'static str> {
        self.send_record(CT_APPDATA, data)
    }

    /// 接收应用数据。返回 Ok(0) = 对端关闭（close_notify）。
    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize, &'static str> {
        if !self.pt_buf.is_empty() {
            let n = buf.len().min(self.pt_buf.len());
            buf[..n].copy_from_slice(&self.pt_buf[..n]);
            self.pt_buf.drain(..n);
            return Ok(n);
        }
        loop {
            if self.peer_closed {
                return Ok(0);
            }
            let (ty, body) = self.read_record()?;
            match ty {
                CT_APPDATA => {
                    // RFC 5246 允许零长度应用数据记录（keepalive/记录边界）：
                    // 返回 Ok(0) 会被上层当 EOF，响应被静默截断 → 继续读。
                    if body.is_empty() {
                        continue;
                    }
                    let n = buf.len().min(body.len());
                    buf[..n].copy_from_slice(&body[..n]);
                    self.pt_buf = body[n..].to_vec();
                    return Ok(n);
                }
                CT_ALERT => {
                    if body.len() < 2 {
                        // 畸形告警（0/1 字节）：索引 body[0]/body[1] 会被远程
                        // 触发 panic（空告警记录）。
                        return Err("TLS 畸形告警记录");
                    }
                    if body[1] == 0 {
                        self.peer_closed = true;
                        return Ok(0);
                    }
                    {
                        tls_log(&format!("TLS 告警: level={} desc={}", body[0], body[1]));
                        return Err("TLS 收到告警");
                    }
                }
                CT_HANDSHAKE => {} // HelloRequest：忽略
                CT_CCS => return Err("TLS 握手期间意外收到 CCS"),
                _ => {}
            }
        }
    }

    /// 关闭：尽力发送 close_notify。
    pub fn close(&mut self) {
        let alert = [CT_ALERT, 0x03, 0x03, 0x00, 0x02, 0x01, 0x00];
        if self.c_enc {
            let seq = self.cseq;
            let mut iv = [0u8; 12];
            iv[..4].copy_from_slice(&self.client_iv);
            iv[4..].copy_from_slice(&seq.to_be_bytes());
            let mut aad = Vec::with_capacity(13);
            aad.extend_from_slice(&seq.to_be_bytes());
            aad.push(CT_ALERT);
            aad.extend_from_slice(&0x0303u16.to_be_bytes());
            let mut pt = [1u8, 0]; // level=warning, desc=close_notify
            // AAD/记录长度按明文实际长度计算（8 nonce + pt + 16 tag）：
            // 硬编码 24 会让对端按 24 字节截取记录 → tag 缺 2 字节解密失败。
            aad.extend_from_slice(&(pt.len() as u16).to_be_bytes());
            let tag = self.client_gcm.seal(&iv, &aad, &mut pt);
            let mut rec = Vec::with_capacity(5 + 8 + pt.len() + 16);
            rec.push(CT_ALERT);
            rec.extend_from_slice(&0x0303u16.to_be_bytes());
            rec.extend_from_slice(&((pt.len() + 24) as u16).to_be_bytes());
            rec.extend_from_slice(&seq.to_be_bytes());
            rec.extend_from_slice(&pt);
            rec.extend_from_slice(&tag);
            let _ = self.tr.send_all(&rec);
            self.cseq += 1;
        } else {
            let _ = self.tr.send_all(&alert);
        }
        self.c_enc = false;
    }

    /// 获取证书链（证书 DER + 签发者 DER）。
    pub fn certs(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.certs
    }
}

// ==================== 握手消息构建/解析 ====================

/// 组装 ClientHello 握手消息（含 4 字节头）。
fn build_client_hello(host: &str, random: &[u8; 32], ecdhe_pub: &[u8; 65]) -> Vec<u8> {
    let mut body = Vec::with_capacity(256);
    body.extend_from_slice(&[0x03, 0x03]); // client_version TLS1.2
    body.extend_from_slice(random);
    body.push(0); // session_id 空
    body.extend_from_slice(&[0x00, 0x02]);
    body.extend_from_slice(&CIPHER_ECDHE_RSA_AES128_GCM_SHA256.to_be_bytes());
    body.extend_from_slice(&[0x01, 0x00]); // compression: null

    let mut ext = Vec::new();
    if is_dns_name(host) {
        let name = host.trim_end_matches('.');
        if !name.is_empty() && name.len() <= 255 {
            let mut sni = Vec::with_capacity(name.len() + 5);
            sni.push(0u8); // host_name
            sni.extend_from_slice(&(name.len() as u16).to_be_bytes());
            sni.extend_from_slice(name.as_bytes());
            ext.extend_from_slice(&0x0000u16.to_be_bytes()); // server_name
            ext.extend_from_slice(&((sni.len() + 2) as u16).to_be_bytes());
            ext.extend_from_slice(&(sni.len() as u16).to_be_bytes());
            ext.extend_from_slice(&sni);
        }
    }
    // supported_groups: secp256r1
    ext.extend_from_slice(&0x000Au16.to_be_bytes());
    ext.extend_from_slice(&4u16.to_be_bytes());
    ext.extend_from_slice(&2u16.to_be_bytes());
    ext.extend_from_slice(&23u16.to_be_bytes());
    // ec_point_formats: uncompressed
    ext.extend_from_slice(&0x000Bu16.to_be_bytes());
    ext.extend_from_slice(&2u16.to_be_bytes());
    ext.push(1);
    ext.push(0);
    // signature_algorithms: rsa_pkcs1_sha256
    ext.extend_from_slice(&0x000Du16.to_be_bytes());
    ext.extend_from_slice(&4u16.to_be_bytes());
    ext.extend_from_slice(&2u16.to_be_bytes());
    ext.extend_from_slice(&0x0401u16.to_be_bytes());
    // renegotiation_info（空，声明 RFC5746 感知）
    ext.extend_from_slice(&0xFF01u16.to_be_bytes());
    ext.extend_from_slice(&1u16.to_be_bytes());
    ext.push(0);

    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);

    let mut msg = Vec::with_capacity(body.len() + 4);
    msg.push(1); // client_hello
    let l = body.len();
    msg.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
    msg.extend_from_slice(&body);
    msg
}

fn record(ty: u8, ver: u16, payload: &[u8]) -> Vec<u8> {
    let mut rec = Vec::with_capacity(5 + payload.len());
    rec.push(ty);
    rec.extend_from_slice(&ver.to_be_bytes());
    rec.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    rec.extend_from_slice(payload);
    rec
}

fn parse_server_hello(b: &[u8], server_random: &mut [u8; 32]) -> Result<(), &'static str> {
    if b.len() < 35 {
        return Err("ServerHello 过短");
    }
    let ver = u16::from_be_bytes([b[0], b[1]]);
    if ver != 0x0303 {
        return Err("服务器不支持 TLS 1.2");
    }
    server_random.copy_from_slice(&b[2..34]);
    let sid_len = b[34] as usize;
    let mut p = 35 + sid_len;
    if b.len() < p + 3 {
        return Err("ServerHello 过短");
    }
    let cipher = u16::from_be_bytes([b[p], b[p + 1]]);
    if cipher != CIPHER_ECDHE_RSA_AES128_GCM_SHA256 {
        return Err("服务器未选择 ECDHE_RSA_AES128_GCM_SHA256 套件");
    }
    p += 2;
    if b[p] != 0 {
        return Err("服务器要求 TLS 压缩");
    }
    Ok(())
}

fn parse_certificate_list(b: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
    if b.len() < 3 {
        return Err("Certificate 消息过短");
    }
    let total = u24(b, 0);
    if total > 96 * 1024 || total + 3 > b.len() {
        return Err("证书列表超限");
    }
    let mut p = 3;
    let end = 3 + total;
    let mut certs = Vec::new();
    while p + 3 <= end {
        let cl = u24(b, p);
        p += 3;
        if cl > 32 * 1024 || p + cl > end {
            return Err("证书长度越界");
        }
        certs.push(b[p..p + cl].to_vec());
        p += cl;
        if certs.len() > MAX_CHAIN {
            return Err("证书链过长");
        }
    }
    Ok(certs)
}

/// 解析 ServerKeyExchange：曲线参数 + 签名。
/// 返回 (对端点, 签名覆盖的参数原始字节, 签名)。
fn parse_server_key_exchange(b: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), &'static str> {
    if b.len() < 4 {
        return Err("ServerKeyExchange 过短");
    }
    if b[0] != 3 {
        return Err("ECDHE 曲线类型不支持");
    }
    let curve = u16::from_be_bytes([b[1], b[2]]);
    if curve != 23 {
        return Err("ECDHE 曲线不支持（仅 secp256r1）");
    }
    let pt_len = b[3] as usize;
    if pt_len != 65 || b.len() < 4 + pt_len {
        return Err("ECDHE 公钥点格式非法");
    }
    let point = b[4..4 + pt_len].to_vec();
    let params_end = 4 + pt_len;
    // signature: hash_alg(1) sig_alg(1) len(2) sig
    if b.len() < params_end + 4 {
        return Err("ServerKeyExchange 签名段过短");
    }
    if b[params_end] != 4 || b[params_end + 1] != 1 {
        return Err("签名算法不支持（仅 SHA256+RSA）");
    }
    let sig_len = u16::from_be_bytes([b[params_end + 2], b[params_end + 3]]) as usize;
    if b.len() < params_end + 4 + sig_len {
        return Err("ServerKeyExchange 签名长度越界");
    }
    let sig = b[params_end + 4..params_end + 4 + sig_len].to_vec();
    let params = b[..params_end].to_vec();
    Ok((point, params, sig))
}
