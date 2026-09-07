//! Wi-Fi 配置管理：已知网络配置文件存储 + 安全静态存储（KeyStore 集成）。
//!
//! - 网络配置文件（`WifiProfile`）：SSID、认证方式（WPA2-PSK / WPA3-SAE /
//!   802.1X-EAP）、隐藏网络、自动加入、优先级；PSK 与 EAP 口令在内存中以明文
//!   保存（会话期间），落盘时由 `seal_store` 加密。
//! - `ProfileStore`：固定数组（无堆）的已知网络表（上限 16）。
//! - 序列化：定长二进制格式（magic + version + 校验），供 /data/wifi.conf 落盘。
//! - 静态加密：AES-KW（RFC 3394）包装 DEK + AES-CTR 加密载荷 + 完整性标签，
//!   主密钥（master key）由 `kunxsec` 的 KeyStore 供给（`seal`/`open` 传入）。
//!
//! 纯函数、无堆、越界返回 `None`、绝不 panic。

use crate::aes::encrypt_block;
use crate::keywrap::{aes_key_unwrap, aes_key_wrap};
use crate::sha2::sha256;

// ==================== 常量/格式 ====================

pub const STORE_MAX: usize = 16;
pub const STORE_MAGIC: [u8; 2] = [b'K', b'W'];
pub const STORE_VERSION: u8 = 1;

/// 单个配置文件定长字节：ssid(32)+len(1)+auth(1)+flags(1)+priority(1)+
/// psk(64)+len(1)+eap_id(64)+len(1)+eap_pass(64)+len(1)+cert_ref(16)。
pub const PROFILE_SIZE: usize = 32 + 1 + 1 + 1 + 1 + 64 + 1 + 64 + 1 + 64 + 1 + 16;
/// 序列化存储定长：magic(2)+version(1)+count(1)+profiles。
pub const STORE_SERIALIZED_SIZE: usize = 4 + STORE_MAX * PROFILE_SIZE;
/// sealed 存储头部：magic(2)+version(1)+nonce(12)+wrapped_dek(40)+tag(8)。
pub const SEALED_HEADER_SIZE: usize = 2 + 1 + 12 + 40 + 8;

// ==================== 网络配置文件 ====================

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WifiProfile {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub auth: u8, // devproto::WIFI_AUTH_*
    pub hidden: bool,
    pub auto_join: bool,
    pub priority: i8,
    /// 会话内存明文 PSK 口令（落盘加密）。
    pub psk: [u8; 64],
    pub psk_len: u8,
    pub eap_identity: [u8; 64],
    pub eap_identity_len: u8,
    pub eap_password: [u8; 64],
    pub eap_password_len: u8,
    /// KeyStore 中的客户端证书引用（802.1X TLS）。
    pub cert_ref: [u8; 16],
}

impl WifiProfile {
    pub fn new(ssid: &[u8], auth: u8, psk: &[u8]) -> Self {
        let mut p = WifiProfile {
            ssid: [0u8; 32],
            ssid_len: 0,
            auth,
            hidden: false,
            auto_join: true,
            priority: 0,
            psk: [0u8; 64],
            psk_len: 0,
            eap_identity: [0u8; 64],
            eap_identity_len: 0,
            eap_password: [0u8; 64],
            eap_password_len: 0,
            cert_ref: [0u8; 16],
        };
        let n = ssid.len().min(32);
        p.ssid[..n].copy_from_slice(&ssid[..n]);
        p.ssid_len = n as u8;
        let n = psk.len().min(64);
        p.psk[..n].copy_from_slice(&psk[..n]);
        p.psk_len = n as u8;
        p
    }

    pub fn ssid_str(&self) -> &[u8] {
        &self.ssid[..self.ssid_len as usize]
    }

    pub fn with_eap(mut self, identity: &[u8], password: &[u8]) -> Self {
        let n = identity.len().min(64);
        self.eap_identity[..n].copy_from_slice(&identity[..n]);
        self.eap_identity_len = n as u8;
        let n = password.len().min(64);
        self.eap_password[..n].copy_from_slice(&password[..n]);
        self.eap_password_len = n as u8;
        self
    }

    pub fn with_flags(mut self, hidden: bool, auto_join: bool, priority: i8) -> Self {
        self.hidden = hidden;
        self.auto_join = auto_join;
        self.priority = priority;
        self
    }

    pub fn with_cert(mut self, cert_ref: &[u8; 16]) -> Self {
        self.cert_ref = *cert_ref;
        self
    }
}

// ==================== 配置存储 ====================

#[derive(Clone, Copy, Debug)]
pub struct ProfileStore {
    pub profiles: [WifiProfile; STORE_MAX],
    pub count: usize,
}

impl ProfileStore {
    pub fn new() -> Self {
        ProfileStore {
            profiles: [WifiProfile::new(b"", 0, b""); STORE_MAX],
            count: 0,
        }
    }

    pub fn find(&self, ssid: &[u8]) -> Option<&WifiProfile> {
        self.profiles
            .iter()
            .take(self.count)
            .find(|p| p.ssid_str() == ssid)
    }

    pub fn find_mut(&mut self, ssid: &[u8]) -> Option<&mut WifiProfile> {
        self.profiles
            .iter_mut()
            .take(self.count)
            .find(|p| p.ssid_str() == ssid)
    }

    /// 新增或更新（同 SSID 覆盖）。
    pub fn upsert(&mut self, p: WifiProfile) -> bool {
        if let Some(ex) = self.find_mut(p.ssid_str()) {
            *ex = p;
            return true;
        }
        if self.count < STORE_MAX {
            self.profiles[self.count] = p;
            self.count += 1;
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, ssid: &[u8]) -> bool {
        let mut i = 0usize;
        while i < self.count {
            if self.profiles[i].ssid_str() == ssid {
                self.profiles[i] = self.profiles[self.count - 1];
                self.count -= 1;
                return true;
            }
            i += 1;
        }
        false
    }

    /// 自动连接目标：auto_join 优先、优先级最高（同优先级取先加入）。
    pub fn auto_join_target(&self) -> Option<&WifiProfile> {
        let mut best: Option<&WifiProfile> = None;
        for p in self.profiles.iter().take(self.count) {
            if !p.auto_join {
                continue;
            }
            match best {
                None => best = Some(p),
                Some(b) if p.priority > b.priority => best = Some(p),
                _ => {}
            }
        }
        best
    }
}

// ==================== 序列化 ====================

fn profile_to_bytes(p: &WifiProfile, out: &mut [u8; PROFILE_SIZE]) {
    let mut off = 0usize;
    out[off..off + 32].copy_from_slice(&p.ssid);
    off += 32;
    out[off] = p.ssid_len;
    off += 1;
    out[off] = p.auth;
    off += 1;
    out[off] = (p.hidden as u8) | ((p.auto_join as u8) << 1);
    off += 1;
    out[off] = p.priority as u8;
    off += 1;
    out[off..off + 64].copy_from_slice(&p.psk);
    off += 64;
    out[off] = p.psk_len;
    off += 1;
    out[off..off + 64].copy_from_slice(&p.eap_identity);
    off += 64;
    out[off] = p.eap_identity_len;
    off += 1;
    out[off..off + 64].copy_from_slice(&p.eap_password);
    off += 64;
    out[off] = p.eap_password_len;
    off += 1;
    out[off..off + 16].copy_from_slice(&p.cert_ref);
}

fn profile_from_bytes(b: &[u8]) -> Option<WifiProfile> {
    if b.len() < PROFILE_SIZE {
        return None;
    }
    let mut p = WifiProfile::new(b"", 0, b"");
    let mut off = 0usize;
    p.ssid.copy_from_slice(&b[off..off + 32]);
    off += 32;
    p.ssid_len = b[off];
    off += 1;
    p.auth = b[off];
    off += 1;
    let flags = b[off];
    p.hidden = flags & 1 != 0;
    p.auto_join = flags & 2 != 0;
    off += 1;
    p.priority = b[off] as i8;
    off += 1;
    p.psk.copy_from_slice(&b[off..off + 64]);
    off += 64;
    p.psk_len = b[off];
    off += 1;
    p.eap_identity.copy_from_slice(&b[off..off + 64]);
    off += 64;
    p.eap_identity_len = b[off];
    off += 1;
    p.eap_password.copy_from_slice(&b[off..off + 64]);
    off += 64;
    p.eap_password_len = b[off];
    off += 1;
    p.cert_ref.copy_from_slice(&b[off..off + 16]);
    Some(p)
}

/// 序列化整个存储（定长，含 count）。
pub fn serialize_store(s: &ProfileStore, out: &mut [u8]) -> Option<usize> {
    if out.len() < STORE_SERIALIZED_SIZE {
        return None;
    }
    out[0] = STORE_MAGIC[0];
    out[1] = STORE_MAGIC[1];
    out[2] = STORE_VERSION;
    out[3] = s.count as u8;
    for i in 0..STORE_MAX {
        let start = 4 + i * PROFILE_SIZE;
        let slot = &mut out[start..start + PROFILE_SIZE];
        let arr: &mut [u8; PROFILE_SIZE] = slot.try_into().ok()?;
        if i < s.count {
            profile_to_bytes(&s.profiles[i], arr);
        } else {
            arr.fill(0);
        }
    }
    Some(STORE_SERIALIZED_SIZE)
}

/// 反序列化存储。
pub fn deserialize_store(data: &[u8]) -> Option<ProfileStore> {
    if data.len() < STORE_SERIALIZED_SIZE
        || data[0] != STORE_MAGIC[0]
        || data[1] != STORE_MAGIC[1]
        || data[2] != STORE_VERSION
    {
        return None;
    }
    let count = (data[3] as usize).min(STORE_MAX);
    let mut s = ProfileStore::new();
    for i in 0..count {
        let start = 4 + i * PROFILE_SIZE;
        let p = profile_from_bytes(&data[start..start + PROFILE_SIZE])?;
        if p.ssid_len == 0 {
            continue;
        }
        s.profiles[s.count] = p;
        s.count += 1;
    }
    Some(s)
}

// ==================== 静态加密（KeyStore 集成） ====================

/// AES-CTR 加密/解密（nonce 12 字节 + 计数器 4 字节 BE，每块 AES-128）。
fn aes_ctr(key: &[u8; 32], nonce: &[u8; 12], data: &[u8], out: &mut [u8]) -> Option<usize> {
    if out.len() < data.len() {
        return None;
    }
    let mut ctr: u32 = 0;
    let mut pos = 0usize;
    while pos < data.len() {
        let mut block = [0u8; 16];
        block[..12].copy_from_slice(nonce);
        block[12..16].copy_from_slice(&ctr.to_be_bytes());
        let k = encrypt_block(key, &block);
        for j in 0..16 {
            if pos + j >= data.len() {
                break;
            }
            out[pos + j] = data[pos + j] ^ k[j];
        }
        pos += 16;
        ctr = ctr.wrapping_add(1);
    }
    Some(data.len())
}

/// 序列化为定长明文（内部辅助）。
fn serialized_plain(s: &ProfileStore) -> [u8; STORE_SERIALIZED_SIZE] {
    let mut b = [0u8; STORE_SERIALIZED_SIZE];
    let _ = serialize_store(s, &mut b);
    b
}

/// 封装（seal）：序列化 → 派生 DEK → AES-KW 包装 → AES-CTR 加密 → 完整性标签。
///
/// 布局：`magic(2) | version(1) | nonce(12) | wrapped_dek(40) | tag(8) | ct`。
/// `master` 为 KeyStore 提供的主密钥（32 字节），`nonce` 为 12 字节随机数
/// （每次落盘重新生成；测试可注入确定性值）。`out` 至少
/// `SEALED_HEADER_SIZE + STORE_SERIALIZED_SIZE`。
pub fn seal_store(
    s: &ProfileStore,
    master: &[u8; 32],
    nonce: &[u8; 12],
    out: &mut [u8],
) -> Option<usize> {
    let total = SEALED_HEADER_SIZE + STORE_SERIALIZED_SIZE;
    if out.len() < total {
        return None;
    }
    // DEK = SHA-256(master || nonce || "kw")[..32]
    let mut seed = [0u8; 32 + 12 + 2];
    seed[..32].copy_from_slice(master);
    seed[32..44].copy_from_slice(nonce);
    seed[44] = b'k';
    seed[45] = b'w';
    let h = sha256(&seed);
    let mut dek = [0u8; 32];
    dek.copy_from_slice(&h[..32]);
    // wrapped DEK = AES-KW(master, DEK)
    let wrapped = aes_key_wrap(master, &dek)?;
    // AES-CTR 加密序列化载荷
    let plain = serialized_plain(s);
    let ct_len = aes_ctr(&dek, nonce, &plain, &mut out[SEALED_HEADER_SIZE..total])?;
    // 头部
    out[0] = STORE_MAGIC[0];
    out[1] = STORE_MAGIC[1];
    out[2] = STORE_VERSION;
    out[3..15].copy_from_slice(nonce);
    out[15..15 + wrapped.len()].copy_from_slice(&wrapped);
    // 完整性标签 = SHA-256(master || ciphertext)[..8]
    let mut tag_in = [0u8; 32 + STORE_SERIALIZED_SIZE];
    tag_in[..32].copy_from_slice(master);
    tag_in[32..].copy_from_slice(&out[SEALED_HEADER_SIZE..SEALED_HEADER_SIZE + STORE_SERIALIZED_SIZE]);
    let tag = sha256(&tag_in);
    out[55..63].copy_from_slice(&tag[..8]);
    Some(SEALED_HEADER_SIZE + ct_len)
}

/// 解封（open）：校验 magic/version/tag，解包 DEK，CTR 解密，反序列化。
pub fn open_store(data: &[u8], master: &[u8; 32]) -> Option<ProfileStore> {
    if data.len() < SEALED_HEADER_SIZE + STORE_SERIALIZED_SIZE
        || data[0] != STORE_MAGIC[0]
        || data[1] != STORE_MAGIC[1]
        || data[2] != STORE_VERSION
    {
        return None;
    }
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&data[3..15]);
    let wrapped = &data[15..55];
    let unwrapped = aes_key_unwrap(master, wrapped)?;
    let mut dek = [0u8; 32];
    dek.copy_from_slice(&unwrapped[..32]);
    let ct = &data[SEALED_HEADER_SIZE..SEALED_HEADER_SIZE + STORE_SERIALIZED_SIZE];
    // 校验标签
    let mut tag_in = [0u8; 32 + STORE_SERIALIZED_SIZE];
    tag_in[..32].copy_from_slice(master);
    tag_in[32..].copy_from_slice(ct);
    let tag = sha256(&tag_in);
    if data[55..63] != tag[..8] {
        return None;
    }
    // CTR 解密
    let mut plain = [0u8; STORE_SERIALIZED_SIZE];
    aes_ctr(&dek, &nonce, ct, &mut plain)?;
    deserialize_store(&plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_add_find_remove() {
        let mut s = ProfileStore::new();
        assert!(s.upsert(WifiProfile::new(b"home", 1, b"secret1").with_flags(false, true, 5)));
        assert!(s.upsert(WifiProfile::new(b"office", 0, b"").with_flags(false, true, 3)));
        assert_eq!(s.count, 2);
        assert_eq!(s.find(b"home").unwrap().priority, 5);
        // 同 SSID 覆盖
        assert!(s.upsert(WifiProfile::new(b"home", 1, b"secret2").with_flags(false, true, 9)));
        assert_eq!(s.count, 2);
        assert_eq!(s.find(b"home").unwrap().psk_len as usize, b"secret2".len());
        assert!(s.remove(b"office"));
        assert_eq!(s.count, 1);
        assert!(!s.remove(b"nonexistent"));
        assert_eq!(s.auto_join_target().unwrap().ssid_str(), b"home");
    }

    #[test]
    fn serialize_roundtrip() {
        let mut s = ProfileStore::new();
        assert!(s.upsert(
            WifiProfile::new(b"kunx", 1, b"0123456789abcdef0123456789abcdef").with_flags(false, true, 5)
        ));
        assert!(s.upsert(
            WifiProfile::new(b"enterprise", 3, b"").with_eap(b"user@kunxos", b"pass")
        ));
        let mut buf = [0u8; STORE_SERIALIZED_SIZE + 8];
        let n = serialize_store(&s, &mut buf).unwrap();
        assert_eq!(n, STORE_SERIALIZED_SIZE);
        let back = deserialize_store(&buf[..n]).unwrap();
        assert_eq!(back.count, 2);
        assert_eq!(back.find(b"kunx").unwrap().ssid_str(), b"kunx");
        assert_eq!(
            back.find(b"enterprise").unwrap().eap_identity_len as usize,
            b"user@kunxos".len()
        );
        assert_eq!(back.auto_join_target().unwrap().ssid_str(), b"kunx");
    }

    #[test]
    fn seal_open_roundtrip_and_tamper() {
        let mut s = ProfileStore::new();
        assert!(s.upsert(WifiProfile::new(b"home", 1, b"0123456789abcdef").with_flags(false, true, 7)));
        let master = [0x42u8; 32];
        let nonce = [0x24u8; 12];
        let mut sealed = [0u8; SEALED_HEADER_SIZE + STORE_SERIALIZED_SIZE + 16];
        let n = seal_store(&s, &master, &nonce, &mut sealed).unwrap();
        assert_eq!(n, SEALED_HEADER_SIZE + STORE_SERIALIZED_SIZE);

        let back = open_store(&sealed[..n], &master).unwrap();
        assert_eq!(back.count, 1);
        assert_eq!(back.find(b"home").unwrap().psk_len as usize, b"0123456789abcdef".len());

        // 篡改 ciphertext → tag 校验失败
        sealed[SEALED_HEADER_SIZE] ^= 0x01;
        assert!(open_store(&sealed[..n], &master).is_none());
        // 错误主密钥 → AES-KW 解包失败
        sealed[SEALED_HEADER_SIZE] ^= 0x01; // 还原
        let bad_master = [0x43u8; 32];
        assert!(open_store(&sealed[..n], &bad_master).is_none());
        // 截断 → None
        assert!(open_store(&sealed[..SEALED_HEADER_SIZE], &master).is_none());
    }
}