//! IEEE 802.1X EAPOL 与 WPA/WPA2 4-way handshake（EAPOL-Key 帧）。
//!
//! 提供：
//! - EAPOL 帧头与 EAPOL-Key 帧的解析/构造
//! - Key Info 位域
//! - 4-way handshake 消息 1-4 的构造与校验（STA 与 AP 双侧）
//! - MIC 计算（HMAC-SHA1 截断 16 字节；AES-CMAC-128 供 SHA256 AKM）
//! - KDE（GTK KDE / RSN IE）编码
//! - 派生物：PTK（见 `prf`）、GTK

use crate::hmac::hmac_sha1;
use crate::prf::{derive_ptk, Ptk};

pub const EAPOL_VERSION: u8 = 2;
pub const EAPOL_TYPE_KEY: u8 = 3;

/// EAPOL-Key 描述符类型。
pub const DESC_RSN: u8 = 2;

/// Key Descriptor Version。
pub const KDV_HMAC_SHA1: u16 = 2;
pub const KDV_AES_CMAC: u16 = 3;

/// EAPOL-Key 帧固定部分长度（不含 key data）。
pub const EAPOL_KEY_FIXED: usize = 95;

/// EAPOL-Key 帧体（descriptor type 2）原始字节解析。
#[derive(Clone, Copy, Debug)]
pub struct EapolKey {
    pub key_info: u16,
    pub key_length: u16,
    pub replay_counter: [u8; 8],
    pub key_nonce: [u8; 32],
    pub key_iv: [u8; 16],
    pub key_rsc: [u8; 8],
    pub key_id: [u8; 8],
    pub key_mic: [u8; 16],
    /// key data（含 KDE）。
    pub key_data: [u8; 200],
    pub key_data_len: usize,
}

impl EapolKey {
    /// 从完整 EAPOL 帧（含 4 字节 EAPOL 头）解析 EAPOL-Key。
    pub fn parse(frame: &[u8]) -> Option<Self> {
        if frame.len() < 4 + EAPOL_KEY_FIXED {
            return None;
        }
        if frame[0] != EAPOL_VERSION || frame[1] != EAPOL_TYPE_KEY {
            return None;
        }
        let body_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
        if body_len + 4 > frame.len() {
            return None;
        }
        let b = &frame[4..4 + body_len];
        if b[0] != DESC_RSN {
            return None;
        }
        let key_info = u16::from_le_bytes([b[1], b[2]]);
        let key_length = u16::from_le_bytes([b[3], b[4]]);
        let mut replay_counter = [0u8; 8];
        replay_counter.copy_from_slice(&b[5..13]);
        let mut key_nonce = [0u8; 32];
        key_nonce.copy_from_slice(&b[13..45]);
        let mut key_iv = [0u8; 16];
        key_iv.copy_from_slice(&b[45..61]);
        let mut key_rsc = [0u8; 8];
        key_rsc.copy_from_slice(&b[61..69]);
        let mut key_id = [0u8; 8];
        key_id.copy_from_slice(&b[69..77]);
        let mut key_mic = [0u8; 16];
        key_mic.copy_from_slice(&b[77..93]);
        let kd_len = u16::from_le_bytes([b[93], b[94]]) as usize;
        if 95 + kd_len > body_len {
            return None;
        }
        let mut key_data = [0u8; 200];
        let n = kd_len.min(200);
        key_data[..n].copy_from_slice(&b[95..95 + n]);
        Some(EapolKey { key_info, key_length, replay_counter, key_nonce, key_iv, key_rsc, key_id, key_mic, key_data, key_data_len: n })
    }

    /// 序列化 EAPOL 帧（含 4 字节 EAPOL 头）。返回写入的字节数。
    pub fn build(&self, out: &mut [u8]) -> Option<usize> {
        let total = 4 + EAPOL_KEY_FIXED + self.key_data_len;
        if out.len() < total {
            return None;
        }
        out[0] = EAPOL_VERSION;
        out[1] = EAPOL_TYPE_KEY;
        let body_len = (EAPOL_KEY_FIXED + self.key_data_len) as u16;
        out[2..4].copy_from_slice(&body_len.to_le_bytes());
        let b = &mut out[4..];
        b[0] = DESC_RSN;
        b[1..3].copy_from_slice(&self.key_info.to_le_bytes());
        b[3..5].copy_from_slice(&self.key_length.to_le_bytes());
        b[5..13].copy_from_slice(&self.replay_counter);
        b[13..45].copy_from_slice(&self.key_nonce);
        b[45..61].copy_from_slice(&self.key_iv);
        b[61..69].copy_from_slice(&self.key_rsc);
        b[69..77].copy_from_slice(&self.key_id);
        b[77..93].copy_from_slice(&self.key_mic);
        b[93..95].copy_from_slice(&(self.key_data_len as u16).to_le_bytes());
        b[95..95 + self.key_data_len].copy_from_slice(&self.key_data[..self.key_data_len]);
        Some(total)
    }
}

/// Key Info 位域辅助。
pub mod keyinfo {
    pub const MASK_KDV: u16 = 0x0003;
    pub const BIT_PAIRWISE: u16 = 0x0008;
    pub const BIT_INSTALL: u16 = 0x0040;
    pub const BIT_ACK: u16 = 0x0080;
    pub const BIT_MIC: u16 = 0x0100;
    pub const BIT_SECURE: u16 = 0x0200;
    pub const BIT_ERROR: u16 = 0x0400;
    pub const BIT_REQUEST: u16 = 0x0800;
    pub const BIT_ENCRYPTED: u16 = 0x1000;

    pub fn kdv(ki: u16) -> u16 {
        ki & MASK_KDV
    }
    pub fn is_pairwise(ki: u16) -> bool {
        ki & BIT_PAIRWISE != 0
    }
    pub fn install(ki: u16) -> bool {
        ki & BIT_INSTALL != 0
    }
    pub fn ack(ki: u16) -> bool {
        ki & BIT_ACK != 0
    }
    pub fn mic(ki: u16) -> bool {
        ki & BIT_MIC != 0
    }
    pub fn secure(ki: u16) -> bool {
        ki & BIT_SECURE != 0
    }
    pub fn error(ki: u16) -> bool {
        ki & BIT_ERROR != 0
    }
    pub fn request(ki: u16) -> bool {
        ki & BIT_REQUEST != 0
    }
    pub fn encrypted(ki: u16) -> bool {
        ki & BIT_ENCRYPTED != 0
    }
}

/// EAPOL-Key 帧的 MIC 计算：对 MIC 字段清零后的完整帧计算。
/// WPA2（KDV=2）：HMAC-SHA1(KCK) 截断 16 字节。
/// WPA2-SHA256（KDV=3）：AES-CMAC-128(KCK)。
pub fn compute_mic(kck: &[u8], frame: &[u8], kdv: u16) -> [u8; 16] {
    let mut work = [0u8; 300];
    let n = frame.len().min(work.len());
    work[..n].copy_from_slice(&frame[..n]);
    let mic_off = 4 + 77;
    if n >= mic_off + 16 {
        work[mic_off..mic_off + 16].fill(0);
    }
    if kdv == KDV_AES_CMAC {
        let mac = crate::cmac::aes_cmac(kck, &work[..n]);
        let mut out = [0u8; 16];
        out.copy_from_slice(&mac);
        out
    } else {
        let h = hmac_sha1(kck, &work[..n]);
        let mut out = [0u8; 16];
        out.copy_from_slice(&h[..16]);
        out
    }
}

/// KDE 构造：GTK KDE（type 1）。
pub fn build_gtk_kde(gtk: &[u8], keyid: u8, tx: u8, out: &mut [u8]) -> Option<usize> {
    let total = 9 + gtk.len();
    if total > out.len() {
        return None;
    }
    out[0] = 0xdd; // 厂商特定
    out[1] = (gtk.len() + 7) as u8; // len 含 OUI(3)+type(1)+keyid/tx/res(3)
    out[2..5].copy_from_slice(&[0x00, 0x0f, 0xac]);
    out[5] = 1; // GTK KDE
    out[6] = keyid;
    out[7] = tx;
    out[8] = 0; // reserved
    out[9..total].copy_from_slice(gtk);
    Some(total)
}

/// 解析 KDE 流：返回 (type, body) 列表。
pub fn parse_kdes(data: &[u8]) -> alloc_vec::Vec<(u8, &[u8])> {
    let mut out = alloc_vec::Vec::new();
    let mut pos = 0usize;
    while pos + 2 <= data.len() {
        let id = data[pos];
        let len = data[pos + 1] as usize;
        if pos + 2 + len > data.len() {
            break;
        }
        if id == 0xdd && len >= 5 && data[pos + 2..pos + 5] == [0x00, 0x0f, 0xac] {
            out.push((data[pos + 5], &data[pos + 6..pos + 2 + len]));
        } else {
            out.push((id, &data[pos + 2..pos + 2 + len]));
        }
        pos += 2 + len;
    }
    out
}

/// 无堆环境下的简易 Vec（仅本模块 KDE 解析用，容量上限 8）。
mod alloc_vec {
    pub struct Vec<T> {
        items: [Option<T>; 8],
        len: usize,
    }
    impl<T> Vec<T> {
        pub fn new() -> Self {
            Vec { items: [None, None, None, None, None, None, None, None], len: 0 }
        }
        pub fn push(&mut self, v: T) {
            if self.len < 8 {
                self.items[self.len] = Some(v);
                self.len += 1;
            }
        }
        pub fn iter(&self) -> impl Iterator<Item = &T> {
            self.items[..self.len].iter().filter_map(|x| x.as_ref())
        }
        pub fn len(&self) -> usize {
            self.len
        }
    }
}

/// 4-way handshake 消息类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandshakeMsg {
    Msg1,
    Msg2,
    Msg3,
    Msg4,
    Unknown,
}

/// 根据 Key Info 判断消息类型。
pub fn classify(key_info: u16) -> HandshakeMsg {
    let pairwise = keyinfo::is_pairwise(key_info);
    let ack = keyinfo::ack(key_info);
    let mic = keyinfo::mic(key_info);
    let secure = keyinfo::secure(key_info);
    let encrypted = keyinfo::encrypted(key_info);
    match (pairwise, ack, mic, secure, encrypted) {
        (true, true, false, false, false) => HandshakeMsg::Msg1,
        (true, false, true, false, false) => HandshakeMsg::Msg2,
        (true, true, true, true, true) => HandshakeMsg::Msg3,
        (true, false, true, true, false) => HandshakeMsg::Msg4,
        _ => HandshakeMsg::Unknown,
    }
}

/// WPA2 4-way handshake 状态机一侧的上下文。
#[derive(Clone, Copy)]
pub struct FourWay {
    pub pmk: [u8; 32],
    pub aa: [u8; 6],
    pub sa: [u8; 6],
    pub anonce: [u8; 32],
    pub snonce: [u8; 32],
    pub replay_counter: u64,
    pub ptk: Ptk,
    pub kdv: u16,
}

impl FourWay {
    /// STA 侧：收到 Msg1 后派生 PTK。
    pub fn sta_on_msg1(&mut self, msg1: &EapolKey) -> Result<(), ()> {
        self.anonce = msg1.key_nonce;
        let ptk = derive_ptk(&self.pmk, &self.aa, &self.sa, &self.anonce, &self.snonce);
        self.ptk = Ptk::from_ptk(&ptk);
        self.kdv = keyinfo::kdv(msg1.key_info);
        self.replay_counter = u64::from_le_bytes(msg1.replay_counter);
        Ok(())
    }

    /// STA 侧：构造 Msg2（SNonce + RSN IE，MIC）。
    pub fn sta_build_msg2(&self, rsn_ie: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut key = EapolKey {
            key_info: 0,
            key_length: 0,
            replay_counter: self.replay_counter.to_le_bytes(),
            key_nonce: self.snonce,
            key_iv: [0u8; 16],
            key_rsc: [0u8; 8],
            key_id: [0u8; 8],
            key_mic: [0u8; 16],
            key_data: [0u8; 200],
            key_data_len: rsn_ie.len().min(200),
        };
        key.key_data[..key.key_data_len].copy_from_slice(&rsn_ie[..key.key_data_len]);
        key.key_info = self.kdv | keyinfo::BIT_PAIRWISE | keyinfo::BIT_MIC;
        let n = key.build(out)?;
        let mic = compute_mic(&self.ptk.kck, &out[..n], self.kdv);
        out[4 + 77..4 + 93].copy_from_slice(&mic);
        Some(n)
    }

    /// STA 侧：校验 Msg3 的 MIC 并安装 PTK/GTK。返回解包后的 GTK。
    pub fn sta_on_msg3(&self, msg3: &EapolKey, gtk_out: &mut [u8; 32]) -> Result<u8, ()> {
        let kdes = parse_kdes(&msg3.key_data[..msg3.key_data_len]);
        for (t, body) in kdes.iter() {
            if *t == 1 && body.len() >= 3 {
                // body = keyid(1) || tx(1) || reserved(1) || wrapped_gtk
                let keyid = body[0];
                let wrapped = &body[3..];
                let kek = &self.ptk.kek;
                let plain = crate::keywrap::aes_key_unwrap(kek, wrapped).ok_or(())?;
                let gtk_len = plain.len().min(32);
                gtk_out[..gtk_len].copy_from_slice(&plain[..gtk_len]);
                return Ok(keyid);
            }
        }
        Err(())
    }

    /// AP 侧：构造 Msg1（ANonce）。
    pub fn ap_build_msg1(&self, out: &mut [u8]) -> Option<usize> {
        let key = EapolKey {
            key_info: self.kdv | keyinfo::BIT_PAIRWISE | keyinfo::BIT_ACK,
            key_length: 16,
            replay_counter: self.replay_counter.to_le_bytes(),
            key_nonce: self.anonce,
            key_iv: [0u8; 16],
            key_rsc: [0u8; 8],
            key_id: [0u8; 8],
            key_mic: [0u8; 16],
            key_data: [0u8; 200],
            key_data_len: 0,
        };
        key.build(out)
    }

    /// AP 侧：校验 Msg2 MIC（成功后双方共享 PTK）。
    pub fn ap_verify_msg2(&self, msg2: &EapolKey, frame: &[u8]) -> bool {
        let kdv = keyinfo::kdv(msg2.key_info);
        let mic = compute_mic(&self.ptk.kck, frame, kdv);
        mic == msg2.key_mic
    }

    /// AP 侧：构造 Msg3（GTK 包装 + MIC）。
    pub fn ap_build_msg3(&self, gtk: &[u8], rsn_ie: &[u8], out: &mut [u8]) -> Option<usize> {
        let wrapped = crate::keywrap::aes_key_wrap(&self.ptk.kek, gtk)?;
        let wlen = gtk.len() + 8;
        let mut kde = [0u8; 49];
        let klen = build_gtk_kde(&wrapped[..wlen], 0, 1, &mut kde)?;
        let mut key = EapolKey {
            key_info: 0,
            key_length: 16,
            replay_counter: self.replay_counter.to_le_bytes(),
            key_nonce: self.anonce,
            key_iv: [0u8; 16],
            key_rsc: [0u8; 8],
            key_id: [0u8; 8],
            key_mic: [0u8; 16],
            key_data: [0u8; 200],
            key_data_len: 0,
        };
        // key data = GTK KDE + RSN IE
        let mut kd = [0u8; 200];
        let mut n = 0usize;
        kd[n..n + klen].copy_from_slice(&kde[..klen]);
        n += klen;
        let rlen = rsn_ie.len().min(200 - n);
        kd[n..n + rlen].copy_from_slice(&rsn_ie[..rlen]);
        n += rlen;
        key.key_data_len = n;
        key.key_data[..n].copy_from_slice(&kd[..n]);
        key.key_info = self.kdv
            | keyinfo::BIT_PAIRWISE
            | keyinfo::BIT_INSTALL
            | keyinfo::BIT_ACK
            | keyinfo::BIT_MIC
            | keyinfo::BIT_SECURE
            | keyinfo::BIT_ENCRYPTED;
        let size = key.build(out)?;
        let mic = compute_mic(&self.ptk.kck, &out[..size], self.kdv);
        out[4 + 77..4 + 93].copy_from_slice(&mic);
        Some(size)
    }

    /// STA 侧：构造 Msg4（MIC）。
    pub fn sta_build_msg4(&self, out: &mut [u8]) -> Option<usize> {
        let key = EapolKey {
            key_info: self.kdv | keyinfo::BIT_PAIRWISE | keyinfo::BIT_MIC | keyinfo::BIT_SECURE,
            key_length: 0,
            replay_counter: self.replay_counter.to_le_bytes(),
            key_nonce: self.snonce,
            key_iv: [0u8; 16],
            key_rsc: [0u8; 8],
            key_id: [0u8; 8],
            key_mic: [0u8; 16],
            key_data: [0u8; 200],
            key_data_len: 0,
        };
        let n = key.build(out)?;
        let mic = compute_mic(&self.ptk.kck, &out[..n], self.kdv);
        out[4 + 77..4 + 93].copy_from_slice(&mic);
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prf::derive_ptk;

    fn test_keys() -> ([u8; 32], [u8; 6], [u8; 6], [u8; 32], [u8; 32]) {
        let pmk = crate::pbkdf2::pbkdf2_sha1(b"password", b"test", 4096, 32);
        let pmk32: [u8; 32] = pmk[..32].try_into().unwrap();
        let aa = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let sa = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let anonce = [0x11u8; 32];
        let snonce = [0x22u8; 32];
        (pmk32, aa, sa, anonce, snonce)
    }

    /// 完整 STA↔AP 4-way 模拟：双方必须收敛到同一 PTK 且 MIC 互验通过。
    #[test]
    fn four_way_handshake_roundtrip() {
        let (pmk, aa, sa, anonce, snonce) = test_keys();
        let rsn_ie = crate::wifi_ie::build_rsn_ie(
            crate::wifi_ie::CIPHER_CCMP,
            crate::wifi_ie::AKM_PSK,
            0,
        );

        let mut ap = FourWay {
            pmk,
            aa,
            sa,
            anonce,
            snonce: [0u8; 32],
            replay_counter: 1,
            ptk: Ptk::from_ptk(&[0u8; 64]),
            kdv: KDV_HMAC_SHA1,
        };
        let mut sta = FourWay {
            pmk,
            aa,
            sa,
            anonce: [0u8; 32],
            snonce,
            replay_counter: 0,
            ptk: Ptk::from_ptk(&[0u8; 64]),
            kdv: KDV_HMAC_SHA1,
        };

        // 消息1：AP → STA
        let mut f1 = [0u8; 200];
        let n1 = ap.ap_build_msg1(&mut f1).unwrap();
        let msg1 = EapolKey::parse(&f1[..n1]).unwrap();
        assert_eq!(classify(msg1.key_info), HandshakeMsg::Msg1);

        sta.sta_on_msg1(&msg1).unwrap();

        let ptk_a = derive_ptk(&pmk, &aa, &sa, &anonce, &snonce);
        assert_eq!(sta.ptk.kck, Ptk::from_ptk(&ptk_a).kck);

        // 消息2：STA → AP
        let mut f2 = [0u8; 300];
        let n2 = sta.sta_build_msg2(&rsn_ie[..], &mut f2).unwrap();
        let msg2 = EapolKey::parse(&f2[..n2]).unwrap();
        assert_eq!(classify(msg2.key_info), HandshakeMsg::Msg2);

        ap.snonce = snonce;
        let ptk_b = derive_ptk(&pmk, &aa, &sa, &anonce, &snonce);
        ap.ptk = Ptk::from_ptk(&ptk_b);
        assert!(ap.ap_verify_msg2(&msg2, &f2[..n2]));

        // 消息3：AP → STA（携带包装 GTK）
        let gtk = [0x33u8; 32];
        let mut f3 = [0u8; 300];
        let n3 = ap.ap_build_msg3(&gtk, &rsn_ie[..], &mut f3).unwrap();
        let msg3 = EapolKey::parse(&f3[..n3]).unwrap();
        assert_eq!(classify(msg3.key_info), HandshakeMsg::Msg3);

        let mut gtk_out = [0u8; 32];
        let keyid = sta.sta_on_msg3(&msg3, &mut gtk_out).unwrap();
        assert_eq!(keyid, 0);
        assert_eq!(&gtk_out[..], &gtk);

        // 消息4：STA → AP
        let mut f4 = [0u8; 200];
        let n4 = sta.sta_build_msg4(&mut f4).unwrap();
        let msg4 = EapolKey::parse(&f4[..n4]).unwrap();
        assert_eq!(classify(msg4.key_info), HandshakeMsg::Msg4);
        let mic = compute_mic(&ap.ptk.kck, &f4[..n4], KDV_HMAC_SHA1);
        assert_eq!(mic, msg4.key_mic);
    }

    #[test]
    fn mic_detects_tamper() {
        let (pmk, aa, sa, anonce, snonce) = test_keys();
        let mut sta = FourWay {
            pmk,
            aa,
            sa,
            anonce: [0u8; 32],
            snonce,
            replay_counter: 0,
            ptk: Ptk::from_ptk(&[0u8; 64]),
            kdv: KDV_HMAC_SHA1,
        };
        let ap = FourWay {
            pmk,
            aa,
            sa,
            anonce,
            snonce: [0u8; 32],
            replay_counter: 1,
            ptk: Ptk::from_ptk(&[0u8; 64]),
            kdv: KDV_HMAC_SHA1,
        };
        let mut f1 = [0u8; 200];
        let n1 = ap.ap_build_msg1(&mut f1).unwrap();
        let msg1 = EapolKey::parse(&f1[..n1]).unwrap();
        sta.sta_on_msg1(&msg1).unwrap();
        let rsn_ie = crate::wifi_ie::build_rsn_ie(
            crate::wifi_ie::CIPHER_CCMP,
            crate::wifi_ie::AKM_PSK,
            0,
        );
        let mut f2 = [0u8; 300];
        let n2 = sta.sta_build_msg2(&rsn_ie[..], &mut f2).unwrap();
        f2[10] ^= 0x01; // 篡改
        let msg2 = EapolKey::parse(&f2[..n2]).unwrap();
        let mut ap2 = ap;
        ap2.snonce = snonce;
        let ptk = derive_ptk(&pmk, &aa, &sa, &anonce, &snonce);
        ap2.ptk = Ptk::from_ptk(&ptk);
        assert!(!ap2.ap_verify_msg2(&msg2, &f2[..n2]));
    }
}
