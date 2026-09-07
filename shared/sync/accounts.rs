//! 账户系统与密钥存储（KeyStore 集成，占位级）。
//!
//! 单文件元数据存储 `<path>`（如 /data/sync/meta.db）：
//!   头 8 字节（magic + 槽位数）+ N × 256 字节槽位。
//!   槽位 tag：0=空闲 1=Account 2=Collection 3=Secret。
//!
//! 密钥存储（`obfuscate`/`deobfuscate`）：异或流混淆，密钥由设备密钥
//! （设备 ID 的 FNV-1a）派生。**这是占位级保护**——与 KunxOS 现有安全
//! 姿态一致（审计已标注 /data 为 XOR 硬编码密钥）；真正的硬件 KeyStore /
//! 安全元件为预留项，见 docs/sync.md「安全」一节。
//!
//! OAuth2 预留：Account.auth=AUTH_OAUTH2 时启用 token_url / client_id /
//! has_refresh_token 字段；初始实现仅 Basic 认证，OAuth2 令牌刷新流
//! 由 sync 引擎的 `oauth_refresh` 钩子预留（当前返回不支持）。

use crate::sync::fs::Fs;
use crate::sync::model::*;
use crate::sync::util;

pub const META_MAGIC: u32 = 0x4B_58_4D_54; // "KXMT"
pub const SLOT_SIZE: usize = 256;
pub const MAX_SLOTS: usize = 48;

pub const TAG_FREE: u8 = 0;
pub const TAG_ACCOUNT: u8 = 1;
pub const TAG_COLLECTION: u8 = 2;
pub const TAG_SECRET: u8 = 3;

// ==================== 迷你序列化器 ====================

struct Ser<'a> {
    b: &'a mut [u8],
    o: usize,
}
impl<'a> Ser<'a> {
    fn new(b: &'a mut [u8]) -> Self {
        Ser { b, o: 0 }
    }
    fn u8(&mut self, v: u8) {
        if self.o < self.b.len() {
            self.b[self.o] = v;
        }
        self.o += 1;
    }
    fn u32(&mut self, v: u32) {
        let bytes = v.to_le_bytes();
        for c in bytes {
            self.u8(c);
        }
    }
    fn u64(&mut self, v: u64) {
        let bytes = v.to_le_bytes();
        for c in bytes {
            self.u8(c);
        }
    }
    fn boolean(&mut self, v: bool) {
        self.u8(if v { 1 } else { 0 });
    }
    fn bytes(&mut self, v: &[u8]) {
        self.u8(v.len().min(255) as u8);
        for &c in v.iter().take(self.b.len().saturating_sub(self.o).min(v.len())) {
            self.b[self.o] = c;
            self.o += 1;
        }
    }
}

struct De<'a> {
    b: &'a [u8],
    o: usize,
}
impl<'a> De<'a> {
    fn new(b: &'a [u8]) -> Self {
        De { b, o: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.o)?;
        self.o += 1;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.b.get(self.o..self.o + 4)?);
        self.o += 4;
        Some(u32::from_le_bytes(b))
    }
    fn u64(&mut self) -> Option<u64> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.b.get(self.o..self.o + 8)?);
        self.o += 8;
        Some(u64::from_le_bytes(b))
    }
    fn boolean(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }
    /// 读取长度前缀字节串到 out，返回实际长度。
    fn bytes(&mut self, out: &mut [u8]) -> Option<usize> {
        let n = self.u8()? as usize;
        let src = self.b.get(self.o..self.o + n)?;
        self.o += n;
        Some(util::copy_into(out, src))
    }
}

// ==================== 序列化 Account / Collection / Secret ====================

fn account_to_slot(a: &Account, slot: &mut [u8; SLOT_SIZE]) {
    let mut s = Ser::new(slot);
    s.u8(TAG_ACCOUNT);
    s.u8(a.id);
    s.bytes(&a.name[..a.name_len]);
    s.u8(a.acct_type);
    s.bytes(&a.server[..a.server_len]);
    s.bytes(&a.principal[..a.principal_len]);
    s.bytes(&a.username[..a.username_len]);
    s.bytes(&a.secret_ref[..a.secret_ref_len]);
    s.u8(a.auth);
    s.boolean(a.enabled);
    s.bytes(&a.token_url[..a.token_url_len]);
    s.bytes(&a.client_id[..a.client_id_len]);
    s.boolean(a.has_refresh_token);
}

fn account_from_slot(slot: &[u8; SLOT_SIZE]) -> Option<Account> {
    if slot[0] != TAG_ACCOUNT {
        return None;
    }
    let mut d = De::new(&slot[1..]);
    let mut a = Account::new();
    a.id = d.u8()?;
    a.name_len = d.bytes(&mut a.name)?;
    a.acct_type = d.u8()?;
    a.server_len = d.bytes(&mut a.server)?;
    a.principal_len = d.bytes(&mut a.principal)?;
    a.username_len = d.bytes(&mut a.username)?;
    a.secret_ref_len = d.bytes(&mut a.secret_ref)?;
    a.auth = d.u8()?;
    a.enabled = d.boolean()?;
    a.token_url_len = d.bytes(&mut a.token_url)?;
    a.client_id_len = d.bytes(&mut a.client_id)?;
    a.has_refresh_token = d.boolean()?;
    Some(a)
}

fn collection_to_slot(c: &Collection, slot: &mut [u8; SLOT_SIZE]) {
    let mut s = Ser::new(slot);
    s.u8(TAG_COLLECTION);
    s.u8(c.id);
    s.u8(c.acct);
    s.u8(c.kind);
    s.bytes(&c.name[..c.name_len]);
    s.u32(c.color);
    s.bytes(&c.path[..c.path_len]);
    s.bytes(&c.ctag[..c.ctag_len]);
    s.bytes(&c.sync_token[..c.sync_token_len]);
    s.u64(c.last_sync);
    s.boolean(c.needs_sync);
    s.boolean(c.dirty);
}

fn collection_from_slot(slot: &[u8; SLOT_SIZE]) -> Option<Collection> {
    if slot[0] != TAG_COLLECTION {
        return None;
    }
    let mut d = De::new(&slot[1..]);
    let mut c = Collection::new();
    c.id = d.u8()?;
    c.acct = d.u8()?;
    c.kind = d.u8()?;
    c.name_len = d.bytes(&mut c.name)?;
    c.color = d.u32()?;
    c.path_len = d.bytes(&mut c.path)?;
    c.ctag_len = d.bytes(&mut c.ctag)?;
    c.sync_token_len = d.bytes(&mut c.sync_token)?;
    c.last_sync = d.u64()?;
    c.needs_sync = d.boolean()?;
    c.dirty = d.boolean()?;
    Some(c)
}

fn secret_to_slot(name: &[u8], value: &[u8], slot: &mut [u8; SLOT_SIZE]) {
    let mut s = Ser::new(slot);
    s.u8(TAG_SECRET);
    s.bytes(name);
    s.bytes(value);
}

fn secret_from_slot(slot: &[u8; SLOT_SIZE], name_out: &mut [u8], value_out: &mut [u8]) -> Option<(usize, usize)> {
    if slot[0] != TAG_SECRET {
        return None;
    }
    let mut d = De::new(&slot[1..]);
    let nl = d.bytes(name_out)?;
    let vl = d.bytes(value_out)?;
    Some((nl, vl))
}

// ==================== 密钥混淆 ====================

/// 用设备密钥对数据进行异或流混淆（对称，可逆）。
/// `key` 由设备 ID 经 FNV-1a 派生（见 `device_key`）。
pub fn obfuscate(key: u32, src: &[u8], dst: &mut [u8]) -> usize {
    let mut x = key.wrapping_mul(0x9E37_79B9).wrapping_add(0x1234_5678);
    let n = src.len().min(dst.len());
    for i in 0..n {
        x = x.wrapping_mul(0x0100_0193).wrapping_add(0x85EB_CA6B);
        dst[i] = src[i] ^ (x >> 24) as u8;
    }
    n
}

/// 与 `obfuscate` 对称。
pub fn deobfuscate(key: u32, src: &[u8], dst: &mut [u8]) -> usize {
    obfuscate(key, src, dst)
}

/// 从设备 ID 派生混淆密钥。
pub fn device_key(device_id: &[u8]) -> u32 {
    util::fnv1a(device_id).wrapping_mul(0x0100_0193) ^ 0x5A17_C0DE
}

// ==================== 元数据存储 ====================

pub struct MetaStore {
    pub accounts: [Account; 8],
    pub n_accounts: usize,
    pub colls: [Collection; 16],
    pub n_colls: usize,
    pub secrets: [([u8; SECRET_NAME_LEN], usize, [u8; SECRET_LEN], usize); 16],
    pub n_secrets: usize,
    path: [u8; 64],
    path_len: usize,
    loaded: bool,
}

impl MetaStore {
    pub fn new(path: &[u8]) -> Self {
        let mut p = [0u8; 64];
        let pl = util::copy_into(&mut p, path);
        MetaStore {
            accounts: [Account::new(); 8],
            n_accounts: 0,
            colls: [Collection::new(); 16],
            n_colls: 0,
            secrets: [([0; SECRET_NAME_LEN], 0, [0; SECRET_LEN], 0); 16],
            n_secrets: 0,
            path: p,
            path_len: pl,
            loaded: false,
        }
    }

    pub fn load(&mut self, fs: &mut dyn Fs) -> bool {
        let mut buf = [0u8; 8 + MAX_SLOTS * SLOT_SIZE];
        let got = fs.read_file(&self.path[..self.path_len], &mut buf);
        if got < 0 {
            self.loaded = true;
            return true;
        }
        let got = got as usize;
        if got < 8 {
            return false;
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != META_MAGIC {
            return false;
        }
        let slots = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if slots > MAX_SLOTS || 8 + slots * SLOT_SIZE > got {
            return false;
        }
        self.n_accounts = 0;
        self.n_colls = 0;
        self.n_secrets = 0;
        for i in 0..slots {
            let mut slot = [0u8; SLOT_SIZE];
            slot.copy_from_slice(&buf[8 + i * SLOT_SIZE..8 + (i + 1) * SLOT_SIZE]);
            match slot[0] {
                TAG_ACCOUNT => {
                    if let Some(a) = account_from_slot(&slot) {
                        if self.n_accounts < self.accounts.len() {
                            self.accounts[self.n_accounts] = a;
                            self.n_accounts += 1;
                        }
                    }
                }
                TAG_COLLECTION => {
                    if let Some(c) = collection_from_slot(&slot) {
                        if self.n_colls < self.colls.len() {
                            self.colls[self.n_colls] = c;
                            self.n_colls += 1;
                        }
                    }
                }
                TAG_SECRET => {
                    if let Some((nl, vl)) = secret_from_slot(&slot, &mut self.secrets[self.n_secrets.min(15)].0, &mut self.secrets[self.n_secrets.min(15)].2) {
                        if self.n_secrets < self.secrets.len() {
                            self.secrets[self.n_secrets].1 = nl;
                            self.secrets[self.n_secrets].3 = vl;
                            self.n_secrets += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        self.loaded = true;
        true
    }

    pub fn save(&mut self, fs: &mut dyn Fs) -> bool {
        let total = self.n_accounts + self.n_colls + self.n_secrets;
        if total > MAX_SLOTS {
            return false;
        }
        let mut buf = [0u8; 8 + MAX_SLOTS * SLOT_SIZE];
        buf[0..4].copy_from_slice(&META_MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&(total as u32).to_le_bytes());
        let mut o = 8usize;
        for i in 0..self.n_accounts {
            let mut slot = [0u8; SLOT_SIZE];
            account_to_slot(&self.accounts[i], &mut slot);
            buf[o..o + SLOT_SIZE].copy_from_slice(&slot);
            o += SLOT_SIZE;
        }
        for i in 0..self.n_colls {
            let mut slot = [0u8; SLOT_SIZE];
            collection_to_slot(&self.colls[i], &mut slot);
            buf[o..o + SLOT_SIZE].copy_from_slice(&slot);
            o += SLOT_SIZE;
        }
        for i in 0..self.n_secrets {
            let mut slot = [0u8; SLOT_SIZE];
            secret_to_slot(&self.secrets[i].0[..self.secrets[i].1], &self.secrets[i].2[..self.secrets[i].3], &mut slot);
            buf[o..o + SLOT_SIZE].copy_from_slice(&slot);
            o += SLOT_SIZE;
        }
        fs.write_file(&self.path[..self.path_len], &buf[..o])
    }

    // ==================== 账户 ====================

    pub fn account(&self, id: u8) -> Option<&Account> {
        self.accounts[..self.n_accounts].iter().find(|a| a.id == id)
    }

    pub fn next_account_id(&self) -> u8 {
        let mut id: u8 = 1;
        loop {
            if self.account(id).is_none() {
                return id;
            }
            id = id.wrapping_add(1);
            if id == 0 {
                return 1;
            }
        }
    }

    pub fn upsert_account(&mut self, a: Account) -> bool {
        if let Some(i) = self.accounts[..self.n_accounts].iter().position(|x| x.id == a.id) {
            self.accounts[i] = a;
            return true;
        }
        if self.n_accounts >= self.accounts.len() {
            return false;
        }
        self.accounts[self.n_accounts] = a;
        self.n_accounts += 1;
        true
    }

    /// 删除账户及其全部集合。
    pub fn remove_account(&mut self, id: u8) {
        let mut i = 0usize;
        while i < self.n_accounts {
            if self.accounts[i].id == id {
                self.accounts.swap(i, self.n_accounts - 1);
                self.n_accounts -= 1;
            } else {
                i += 1;
            }
        }
        let mut j = 0usize;
        while j < self.n_colls {
            if self.colls[j].acct == id {
                self.colls.swap(j, self.n_colls - 1);
                self.n_colls -= 1;
            } else {
                j += 1;
            }
        }
    }

    // ==================== 集合 ====================

    pub fn collection(&self, id: u8) -> Option<&Collection> {
        self.colls[..self.n_colls].iter().find(|c| c.id == id)
    }

    pub fn collection_mut(&mut self, id: u8) -> Option<&mut Collection> {
        self.colls[..self.n_colls].iter_mut().find(|c| c.id == id)
    }

    pub fn next_coll_id(&self) -> u8 {
        let mut id: u8 = 1;
        loop {
            if self.collection(id).is_none() {
                return id;
            }
            id = id.wrapping_add(1);
            if id == 0 {
                return 1;
            }
        }
    }

    pub fn upsert_collection(&mut self, c: Collection) -> bool {
        if let Some(i) = self.colls[..self.n_colls].iter().position(|x| x.id == c.id) {
            self.colls[i] = c;
            return true;
        }
        if self.n_colls >= self.colls.len() {
            return false;
        }
        self.colls[self.n_colls] = c;
        self.n_colls += 1;
        true
    }

    pub fn remove_collection(&mut self, id: u8) {
        let mut i = 0usize;
        while i < self.n_colls {
            if self.colls[i].id == id {
                self.colls.swap(i, self.n_colls - 1);
                self.n_colls -= 1;
            } else {
                i += 1;
            }
        }
    }

    // ==================== 密钥 ====================

    /// 存储密钥（混淆后落盘）。
    pub fn secret_set(&mut self, name: &[u8], value: &[u8], dev_key: u32) -> bool {
        if let Some(i) = self.secrets[..self.n_secrets].iter().position(|(n, nl, _, _)| &n[..*nl] == name) {
            let mut ob = [0u8; SECRET_LEN];
            let ol = obfuscate(dev_key, value, &mut ob);
            self.secrets[i].2 = [0; SECRET_LEN];
            self.secrets[i].3 = ol;
            self.secrets[i].2[..ol].copy_from_slice(&ob[..ol]);
            return true;
        }
        if self.n_secrets >= self.secrets.len() {
            return false;
        }
        let mut ob = [0u8; SECRET_LEN];
        let ol = obfuscate(dev_key, value, &mut ob);
        self.secrets[self.n_secrets].0 = [0; SECRET_NAME_LEN];
        self.secrets[self.n_secrets].1 = util::copy_into(&mut self.secrets[self.n_secrets].0, name);
        self.secrets[self.n_secrets].2 = [0; SECRET_LEN];
        self.secrets[self.n_secrets].3 = ol;
        self.secrets[self.n_secrets].2[..ol].copy_from_slice(&ob[..ol]);
        self.n_secrets += 1;
        true
    }

    /// 读取密钥（解密），返回长度；未找到返回 0。
    pub fn secret_get(&self, name: &[u8], dev_key: u32, out: &mut [u8]) -> usize {
        for i in 0..self.n_secrets {
            let (n, nl, v, vl) = (&self.secrets[i].0, self.secrets[i].1, &self.secrets[i].2, self.secrets[i].3);
            if &n[..nl] == name {
                return deobfuscate(dev_key, &v[..vl], out);
            }
        }
        0
    }

    /// 账户密码的约定密钥条目名。
    pub fn account_secret_name(acct_id: u8, buf: &mut [u8; 16]) -> usize {
        let mut n = util::copy_into(buf, b"acct:");
        n = util::push_dec(buf, n, acct_id as u64);
        n
    }

    /// 账户密码的约定刷新令牌条目名。
    pub fn refresh_secret_name(acct_id: u8, buf: &mut [u8; 16]) -> usize {
        let mut n = util::copy_into(buf, b"refresh:");
        n = util::push_dec(buf, n, acct_id as u64);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MemFs {
        files: HashMap<Vec<u8>, Vec<u8>>,
    }
    impl MemFs {
        fn new() -> Self {
            MemFs { files: HashMap::new() }
        }
    }
    impl Fs for MemFs {
        fn read_file(&mut self, path: &[u8], buf: &mut [u8]) -> isize {
            match self.files.get(path) {
                Some(d) => {
                    let n = d.len().min(buf.len());
                    buf[..n].copy_from_slice(&d[..n]);
                    n as isize
                }
                None => -1,
            }
        }
        fn write_file(&mut self, path: &[u8], data: &[u8]) -> bool {
            self.files.insert(path.to_vec(), data.to_vec());
            true
        }
        fn write_file_at(&mut self, path: &[u8], off: u32, data: &[u8]) -> bool {
            let e = self.files.entry(path.to_vec()).or_default();
            let end = off as usize + data.len();
            if e.len() < end {
                e.resize(end, 0);
            }
            e[off as usize..end].copy_from_slice(data);
            true
        }
        fn delete_file(&mut self, path: &[u8]) -> bool {
            self.files.remove(path);
            true
        }
        fn file_exists(&mut self, path: &[u8]) -> bool {
            self.files.contains_key(path)
        }
    }

    fn sample_account(id: u8) -> Account {
        let mut a = Account::new();
        a.id = id;
        a.name_len = util::copy_into(&mut a.name, b"Google");
        a.acct_type = ACCT_GOOGLE;
        a.server_len = util::copy_into(&mut a.server, b"https://apidata.googleusercontent.com");
        a.principal_len = util::copy_into(&mut a.principal, b"/caldav/v2/");
        a.username_len = util::copy_into(&mut a.username, b"user@gmail.com");
        a.auth = AUTH_BASIC;
        a.enabled = true;
        a
    }

    #[test]
    fn meta_roundtrip() {
        let mut fs = MemFs::new();
        {
            let mut m = MetaStore::new(b"/data/sync/meta.db");
            m.load(&mut fs);
            let mut a = sample_account(1);
            m.upsert_account(a);
            let mut c = Collection::new();
            c.id = 1;
            c.acct = 1;
            c.kind = COLL_CALENDAR;
            c.name_len = util::copy_into(&mut c.name, b"Personal");
            c.path_len = util::copy_into(&mut c.path, b"/dav/calendars/u/cal1/");
            c.sync_token_len = util::copy_into(&mut c.sync_token, b"token-123");
            c.ctag_len = util::copy_into(&mut c.ctag, b"ctag-9");
            m.upsert_collection(c);
            assert!(m.secret_set(b"acct:1", b"secret-password", 0xdeadbeef));
            assert!(m.save(&mut fs));
            a.acct_type = ACCT_ICLOUD; // 修改后验证重载还原
            m.upsert_account(a);
        }
        let mut m2 = MetaStore::new(b"/data/sync/meta.db");
        assert!(m2.load(&mut fs));
        assert_eq!(m2.n_accounts, 1);
        let a = m2.account(1).unwrap();
        assert_eq!(&a.name[..a.name_len], b"Google");
        assert_eq!(a.acct_type, ACCT_GOOGLE);
        assert_eq!(&a.server[..a.server_len], b"https://apidata.googleusercontent.com");
        assert_eq!(m2.n_colls, 1);
        let c = m2.collection(1).unwrap();
        assert_eq!(&c.sync_token[..c.sync_token_len], b"token-123");
        assert_eq!(&c.ctag[..c.ctag_len], b"ctag-9");
        let mut pw = [0u8; SECRET_LEN];
        let pl = m2.secret_get(b"acct:1", 0xdeadbeef, &mut pw);
        assert_eq!(&pw[..pl], b"secret-password");
        // 错误密钥解不出原文
        let mut pw2 = [0u8; SECRET_LEN];
        let pl2 = m2.secret_get(b"acct:1", 0, &mut pw2);
        assert_ne!(&pw2[..pl2], b"secret-password");
    }

    #[test]
    fn obfuscate_symmetric() {
        let mut enc = [0u8; 64];
        let n = obfuscate(0x12345678, b"hunter2", &mut enc);
        let mut dec = [0u8; 64];
        let m = deobfuscate(0x12345678, &enc[..n], &mut dec);
        assert_eq!(&dec[..m], b"hunter2");
        let mut dec2 = [0u8; 64];
        let m2 = deobfuscate(0x99999999, &enc[..n], &mut dec2);
        assert_ne!(&dec2[..m2], b"hunter2");
    }

    #[test]
    fn remove_account_cascades() {
        let mut fs = MemFs::new();
        let mut m = MetaStore::new(b"m");
        m.load(&mut fs);
        m.upsert_account(sample_account(1));
        let mut c = Collection::new();
        c.id = 1;
        c.acct = 1;
        c.kind = COLL_ADDRBOOK;
        m.upsert_collection(c);
        assert_eq!(m.n_colls, 1);
        m.remove_account(1);
        assert_eq!(m.n_accounts, 0);
        assert_eq!(m.n_colls, 0);
    }

    #[test]
    fn id_allocation() {
        let mut m = MetaStore::new(b"m");
        assert_eq!(m.next_account_id(), 1);
        m.upsert_account(sample_account(1));
        assert_eq!(m.next_account_id(), 2);
        assert_eq!(m.next_coll_id(), 1);
    }
}
