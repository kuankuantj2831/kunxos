//! Wi-Fi 站点（STA）管理状态机：扫描 / 连接 / 断开 / 基础漫游。
//!
//! 覆盖：
//! - 扫描状态机与扫描表（纯逻辑，QEMU 模拟后端注入结果）
//! - 连接状态机：SSID 发现 → 认证（开放 / SAE）→ 关联 → 4-way handshake
//!   （WPA2-PSK / WPA3-SAE / 802.1X-EAP）
//! - 断开与重连
//! - 基础漫游：连接后 RSSI 低于阈值且存在更优 BSS 时触发重连
//!   （802.11r FT 不在初始范围，见 docs/wireless.md）
//!
//! 状态/认证码与 `devproto`（/dev/wifi 内核↔用户协议）对齐；SAE/4-way/EAP
//! 载荷由驱动层编帧到 802.11 帧（Auth/Assoc/EAPOL），本模块只负责状态与密钥
//! 逻辑。纯函数、无堆、绝不 panic。

use crate::devproto as dp;
use crate::eap::{self, EapPacket};
#[cfg(test)]
use crate::eap::EapAuthenticator;
use crate::eapol::{self, EapolKey, FourWay, HandshakeMsg, KDV_HMAC_SHA1};
use crate::p256::Point;
use crate::sae::{self, SaeSession};
use crate::wifi_ie as ie;

// ==================== 扫描结果 ====================

/// 扫描结果（字段与 `/dev/wifi` 的 `WifiScanRecord` 一致）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanEntry {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub bssid: [u8; 6],
    pub channel: u8,
    pub rssi: i8,
    pub auth: u8,
    pub hidden: bool,
}

impl ScanEntry {
    pub fn new(ssid: &[u8], bssid: [u8; 6], channel: u8, rssi: i8, auth: u8, hidden: bool) -> Self {
        let mut e = ScanEntry {
            ssid: [0u8; 32],
            ssid_len: ssid.len().min(32) as u8,
            bssid,
            channel,
            rssi,
            auth,
            hidden,
        };
        let n = e.ssid_len as usize;
        e.ssid[..n].copy_from_slice(&ssid[..n]);
        e
    }

    pub fn ssid_str(&self) -> &[u8] {
        &self.ssid[..self.ssid_len as usize]
    }
}

/// 待发送载荷（定长缓冲 + 有效长度）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Payload<const N: usize> {
    pub data: [u8; N],
    pub len: usize,
}

impl<const N: usize> Payload<N> {
    pub fn from_slice(d: &[u8]) -> Option<Self> {
        if d.len() > N {
            return None;
        }
        let mut data = [0u8; N];
        data[..d.len()].copy_from_slice(d);
        Some(Payload { data, len: d.len() })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

// ==================== 阶段 / 动作 ====================

/// 连接阶段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConnectPhase {
    Idle,
    Auth,
    SaeCommit,
    SaeConfirm,
    Assoc,
    Eap,
    FourWay,
    Connected,
}

/// 漫游决策。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RoamDecision {
    None,
    Handover { bssid: [u8; 6], channel: u8 },
}

/// 一次推进产生的输出动作（供驱动/wifid 执行）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WifiAction {
    None,
    /// 发起扫描（参数：主动探测）。
    Scan(bool),
    /// 发送认证帧（算法, 事务序号, 状态码）。
    SendAuth(u16, u16, u16),
    /// 发送 SAE Commit（96 字节）。
    SendSaeCommit(Payload<96>),
    /// 发送 SAE Confirm（32 字节）。
    SendSaeConfirm(Payload<32>),
    /// 发送关联请求。
    SendAssoc,
    /// 发送 EAPOL-Start（802.1X）。
    SendEapStart,
    /// 发送 EAPOL-Key 帧（4-way）。
    SendEapol(Payload<400>),
    /// 发送 EAP 响应。
    SendEap(Payload<320>),
    /// 发送去关联帧。
    SendDisassoc,
    /// 已连接。
    Connected,
    /// 已断开。
    Disconnected,
    /// 错误（编号见 ERROR_*）。。
    Error(u8),
}

pub const ERROR_NO_PROFILE: u8 = 1;
pub const ERROR_SAE: u8 = 2;
pub const ERROR_AUTH: u8 = 3;
pub const ERROR_ASSOC: u8 = 4;
pub const ERROR_UNSUPPORTED: u8 = 5;
pub const ERROR_HANDSHAKE: u8 = 6;
pub const ERROR_EAP: u8 = 7;

// ==================== 站点状态机 ====================

pub const SCAN_MAX: usize = 16;
pub const RSSI_ROAM_THRESHOLD: i8 = -78;
pub const RSSI_BETTER_MARGIN: i8 = 6;

pub struct WifiSta {
    pub state: u8,
    pub phase: ConnectPhase,
    pub scan_table: [ScanEntry; SCAN_MAX],
    pub scan_count: usize,

    pub target_ssid: [u8; 32],
    pub target_ssid_len: u8,
    pub target_bssid: [u8; 6],
    pub target_auth: u8,
    pub psk: [u8; 64],
    pub psk_len: u8,
    pub eap_identity: [u8; 64],
    pub eap_identity_len: u8,
    pub eap_password: [u8; 64],
    pub eap_password_len: u8,
    pub eap_method: u8, // 0=MD5 1=TLS（骨架）
    pub eap_state: u8,

    pub pmk: [u8; 32],
    pub handshake: Option<FourWay>,
    pub sae: Option<SaeSession>,
    pub sae_rand: [u8; 32],
    pub sae_mask: [u8; 32],
    /// 对端（AP）SAE commit 的 scalar/element（校验 confirm 用）。
    pub sae_peer_scalar: [u8; 32],
    pub sae_peer_elem: Point,

    pub rsn_caps: u16,
    pub current_rssi: i8,
    pub connected_bssid: [u8; 6],
    /// 本端 MAC（驱动注入；SAE/4-way 需要）。
    pub own_mac: [u8; 6],
    pub reconnect_attempts: u8,
    pub auto_connect: bool,
}

impl WifiSta {
    pub fn new() -> Self {
        WifiSta {
            state: dp::WIFI_STATE_IDLE,
            phase: ConnectPhase::Idle,
            scan_table: [ScanEntry::new(b"", [0; 6], 0, 0, dp::WIFI_AUTH_OPEN, false); SCAN_MAX],
            scan_count: 0,
            target_ssid: [0u8; 32],
            target_ssid_len: 0,
            target_bssid: [0u8; 6],
            target_auth: dp::WIFI_AUTH_OPEN,
            psk: [0u8; 64],
            psk_len: 0,
            eap_identity: [0u8; 64],
            eap_identity_len: 0,
            eap_password: [0u8; 64],
            eap_password_len: 0,
            eap_method: 0,
            eap_state: 0,
            pmk: [0u8; 32],
            handshake: None,
            sae: None,
            sae_rand: [0u8; 32],
            sae_mask: [0u8; 32],
            sae_peer_scalar: [0u8; 32],
            sae_peer_elem: Point::new([0; 4], [0; 4]),
            rsn_caps: 0,
            current_rssi: 0,
            connected_bssid: [0u8; 6],
            own_mac: [0x02, 0, 0, 0, 0, 0x02],
            reconnect_attempts: 0,
            auto_connect: true,
        }
    }

    pub fn set_eap_credentials(&mut self, identity: &[u8], password: &[u8]) {
        let n = identity.len().min(64);
        self.eap_identity[..n].copy_from_slice(&identity[..n]);
        self.eap_identity_len = n as u8;
        let n = password.len().min(64);
        self.eap_password[..n].copy_from_slice(&password[..n]);
        self.eap_password_len = n as u8;
    }

    pub fn set_psk(&mut self, psk: &[u8]) {
        let n = psk.len().min(64);
        self.psk[..n].copy_from_slice(&psk[..n]);
        self.psk_len = n as u8;
    }

    /// 发起扫描（active=主动探测）。
    pub fn scan(&mut self, active: bool) -> WifiAction {
        self.state = dp::WIFI_STATE_SCANNING;
        self.scan_count = 0;
        WifiAction::Scan(active)
    }

    /// 注入一条扫描结果。
    pub fn scan_result(&mut self, e: ScanEntry) -> bool {
        if self.scan_count < SCAN_MAX {
            self.scan_table[self.scan_count] = e;
            self.scan_count += 1;
            true
        } else {
            false
        }
    }

    /// 返回扫描表中与 SSID 匹配、信号最强的一项。
    pub fn pick_best_bss(&self, ssid: &[u8]) -> Option<&ScanEntry> {
        let mut best: Option<&ScanEntry> = None;
        for e in self.scan_table.iter().take(self.scan_count) {
            if e.ssid_str() == ssid {
                match best {
                    None => best = Some(e),
                    Some(b) if e.rssi > b.rssi => best = Some(e),
                    _ => {}
                }
            }
        }
        best
    }

    /// 扫描完成：自动连接已配置网络（信号最强匹配项）。
    pub fn scan_done(&mut self) -> WifiAction {
        if !self.auto_connect || self.target_ssid_len == 0 {
            self.state = dp::WIFI_STATE_IDLE;
            return WifiAction::None;
        }
        let want = self.target_ssid_str();
        let mut best: Option<usize> = None;
        for i in 0..self.scan_count {
            if self.scan_table[i].ssid_str() == want {
                match best {
                    None => best = Some(i),
                    Some(b) if self.scan_table[i].rssi > self.scan_table[b].rssi => best = Some(i),
                    _ => {}
                }
            }
        }
        match best {
            Some(i) => {
                let e = self.scan_table[i];
                self.target_bssid = e.bssid;
                self.target_auth = e.auth;
                if self.target_auth == dp::WIFI_AUTH_WPA2_PSK && self.psk_len == 0 {
                    return WifiAction::Error(ERROR_NO_PROFILE);
                }
                self.current_rssi = e.rssi;
                begin_auth(self, e)
            }
            None => WifiAction::None,
        }
    }

    fn target_ssid_str(&self) -> &[u8] {
        &self.target_ssid[..self.target_ssid_len as usize]
    }

    /// 显式连接。`bssid=None` 时自动选择信号最强 BSS（无结果则触发扫描）。
    pub fn connect(&mut self, ssid: &[u8], auth: u8, bssid: Option<[u8; 6]>) -> WifiAction {
        let n = ssid.len().min(32);
        self.target_ssid[..n].copy_from_slice(&ssid[..n]);
        self.target_ssid_len = n as u8;
        self.target_auth = auth;
        if auth == dp::WIFI_AUTH_WPA2_PSK && self.psk_len == 0 {
            return WifiAction::Error(ERROR_NO_PROFILE);
        }
        let entry = match bssid {
            Some(b) => ScanEntry::new(ssid, b, 0, 0, auth, false),
            None => match self.pick_best_bss(ssid) {
                Some(e) => *e,
                None => return self.scan(true),
            },
        };
        self.target_bssid = entry.bssid;
        self.current_rssi = entry.rssi;
        begin_auth(self, entry)
    }

    /// 初始化 SAE 会话（WPA3），生成 Commit。`own_mac` 由驱动注入。
    pub fn sae_begin(&mut self, own_mac: &[u8; 6]) -> WifiAction {
        let pw = &self.psk[..self.psk_len as usize];
        let pwe = match sae::derive_pwe(own_mac, &self.target_bssid, pw) {
            Some((pwe, _)) => pwe,
            None => return WifiAction::Error(ERROR_SAE),
        };
        let lrand = if self.sae_rand == [0u8; 32] { [0x11u8; 32] } else { self.sae_rand };
        let lmask = if self.sae_mask == [0u8; 32] { [0x22u8; 32] } else { self.sae_mask };
        let (sess, commit) = sae::make_commit(&lrand, &lmask, &pwe);
        self.sae = Some(sess);
        self.sae_rand = lrand;
        self.sae_mask = lmask;
        self.phase = ConnectPhase::SaeCommit;
        match Payload::<96>::from_slice(&commit) {
            Some(p) => WifiAction::SendSaeCommit(p),
            None => WifiAction::Error(ERROR_SAE),
        }
    }

    /// 收到 AP 的 SAE Commit → 派生 KCK/PMK，构造 Confirm。
    pub fn on_sae_commit(&mut self, peer_commit: &[u8]) -> WifiAction {
        let (scalar, elem) = match sae::parse_commit(peer_commit) {
            Some(v) => v,
            None => return WifiAction::Error(ERROR_SAE),
        };
        if self.sae.is_none() {
            return WifiAction::Error(ERROR_SAE);
        }
        let rand = self.sae_rand;
        let sess = &mut self.sae;
        if !sae::on_peer_commit(sess.as_mut().unwrap(), &rand, &scalar, &elem) {
            return WifiAction::Error(ERROR_SAE);
        }
        self.sae_peer_scalar = scalar;
        self.sae_peer_elem = elem;
        let confirm = {
            let s = sess.as_ref().unwrap();
            sae::make_confirm(s, &scalar, &elem)
        };
        self.phase = ConnectPhase::SaeConfirm;
        match Payload::<32>::from_slice(&confirm) {
            Some(p) => WifiAction::SendSaeConfirm(p),
            None => WifiAction::Error(ERROR_SAE),
        }
    }

    /// 收到 AP 的 SAE Confirm → 校验，成功后进入关联。
    pub fn on_sae_confirm(&mut self, peer_confirm: &[u8], peer_sc: u16) -> WifiAction {
        if peer_confirm.len() < 32 || self.sae.is_none() {
            return WifiAction::Error(ERROR_SAE);
        }
        let mut pc = [0u8; 32];
        pc.copy_from_slice(&peer_confirm[..32]);
        let sess = self.sae.unwrap();
        if !sae::verify_confirm(
            &sess,
            peer_sc,
            &self.sae_peer_scalar,
            &self.sae_peer_elem,
            &pc,
        ) {
            return WifiAction::Error(ERROR_SAE);
        }
        self.pmk = sess.pmk;
        self.phase = ConnectPhase::Assoc;
        self.state = dp::WIFI_STATE_CONNECTING;
        WifiAction::SendAssoc
    }

    /// 认证结果（开放系统认证帧的 status）。
    pub fn on_auth_result(&mut self, status: u16) -> WifiAction {
        if status != ie::STATUS_SUCCESS {
            return WifiAction::Error(ERROR_AUTH);
        }
        self.phase = ConnectPhase::Assoc;
        self.state = dp::WIFI_STATE_CONNECTING;
        WifiAction::SendAssoc
    }

    /// 关联结果。
    pub fn on_assoc_result(&mut self, status: u16, caps: u16) -> WifiAction {
        if status != ie::STATUS_SUCCESS {
            return WifiAction::Error(ERROR_ASSOC);
        }
        self.rsn_caps = caps;
        match self.target_auth {
            dp::WIFI_AUTH_OPEN => {
                self.state = dp::WIFI_STATE_CONNECTED;
                self.phase = ConnectPhase::Connected;
                self.connected_bssid = self.target_bssid;
                WifiAction::Connected
            }
            dp::WIFI_AUTH_WPA2_PSK | dp::WIFI_AUTH_WPA3_SAE | dp::WIFI_AUTH_8021X => {
                self.state = dp::WIFI_STATE_HANDSHAKE;
                self.phase = ConnectPhase::FourWay;
                if self.target_auth == dp::WIFI_AUTH_8021X {
                    self.phase = ConnectPhase::Eap;
                    WifiAction::SendEapStart
                } else {
                    if self.target_auth != dp::WIFI_AUTH_WPA3_SAE {
                        self.derive_psk_pmk();
                    }
                    WifiAction::None
                }
            }
            _ => WifiAction::Error(ERROR_UNSUPPORTED),
        }
    }

    fn derive_psk_pmk(&mut self) {
        let out = crate::pbkdf2::pbkdf2_sha1(
            &self.psk[..self.psk_len as usize],
            self.target_ssid_str(),
            4096,
            32,
        );
        self.pmk.copy_from_slice(&out[..32]);
    }

    /// 建立 4-way 上下文（收到 Msg1 前初始化）。
    fn ensure_four_way(&mut self, own_mac: &[u8; 6]) {
        if self.handshake.is_none() {
            self.handshake = Some(FourWay {
                pmk: self.pmk,
                aa: self.target_bssid,
                sa: *own_mac,
                anonce: [0u8; 32],
                snonce: [0x33u8; 32],
                replay_counter: 0,
                ptk: crate::prf::Ptk::from_ptk(&[0u8; 64]),
                kdv: KDV_HMAC_SHA1,
            });
        }
    }

    /// 收到 EAPOL-Key（Msg1/Msg3）。`own_mac` 由驱动注入。
    pub fn on_eapol_key(&mut self, frame: &[u8], own_mac: &[u8; 6]) -> WifiAction {
        if self.state != dp::WIFI_STATE_HANDSHAKE || self.phase != ConnectPhase::FourWay {
            return WifiAction::Error(ERROR_HANDSHAKE);
        }
        let msg = match EapolKey::parse(frame) {
            Some(m) => m,
            None => return WifiAction::Error(ERROR_HANDSHAKE),
        };
        self.ensure_four_way(own_mac);
        let fw = match &mut self.handshake {
            Some(f) => f,
            None => return WifiAction::Error(ERROR_HANDSHAKE),
        };
        match eapol::classify(msg.key_info) {
            HandshakeMsg::Msg1 => {
                if fw.sta_on_msg1(&msg).is_err() {
                    return WifiAction::Error(ERROR_HANDSHAKE);
                }
                let akm = match self.target_auth {
                    dp::WIFI_AUTH_WPA3_SAE => ie::AKM_SAE,
                    dp::WIFI_AUTH_8021X => ie::AKM_8021X,
                    _ => ie::AKM_PSK,
                };
                let rsn = ie::build_rsn_ie(ie::CIPHER_CCMP, akm, 0);
                let mut buf = [0u8; 400];
                let n = match fw.sta_build_msg2(&rsn, &mut buf) {
                    Some(n) => n,
                    None => return WifiAction::Error(ERROR_HANDSHAKE),
                };
                match Payload::<400>::from_slice(&buf[..n]) {
                    Some(p) => WifiAction::SendEapol(p),
                    None => WifiAction::Error(ERROR_HANDSHAKE),
                }
            }
            HandshakeMsg::Msg3 => {
                let mut gtk = [0u8; 32];
                if fw.sta_on_msg3(&msg, &mut gtk).is_err() {
                    return WifiAction::Error(ERROR_HANDSHAKE);
                }
                let mut buf = [0u8; 400];
                let n = match fw.sta_build_msg4(&mut buf) {
                    Some(n) => n,
                    None => return WifiAction::Error(ERROR_HANDSHAKE),
                };
                let pay = match Payload::<400>::from_slice(&buf[..n]) {
                    Some(p) => p,
                    None => return WifiAction::Error(ERROR_HANDSHAKE),
                };
                self.state = dp::WIFI_STATE_CONNECTED;
                self.phase = ConnectPhase::Connected;
                self.connected_bssid = self.target_bssid;
                WifiAction::SendEapol(pay)
            }
            _ => WifiAction::Error(ERROR_HANDSHAKE),
        }
    }

    /// 802.1X：收到 EAP Request → 构造响应。
    pub fn on_eap_request(&mut self, pkt: &EapPacket) -> WifiAction {
        let mut buf = [0u8; 320];
        match pkt.etype {
            eap::EAP_TYPE_IDENTITY => {
                let n = match eap::build_identity_response(
                    pkt.id,
                    &self.eap_identity[..self.eap_identity_len as usize],
                    &mut buf,
                ) {
                    Some(n) => n,
                    None => return WifiAction::Error(ERROR_EAP),
                };
                self.eap_state = 1;
                match Payload::<320>::from_slice(&buf[..n]) {
                    Some(p) => WifiAction::SendEap(p),
                    None => WifiAction::Error(ERROR_EAP),
                }
            }
            eap::EAP_TYPE_MD5 => {
                let ch = match eap::parse_md5_request(&pkt.data[..pkt.data_len]) {
                    Some(c) => c,
                    None => return WifiAction::Error(ERROR_EAP),
                };
                let pass = &self.eap_password[..self.eap_password_len as usize];
                let n = match eap::build_md5_response(pkt.id, pass, ch, b"", &mut buf) {
                    Some(n) => n,
                    None => return WifiAction::Error(ERROR_EAP),
                };
                self.eap_state = 2;
                match Payload::<320>::from_slice(&buf[..n]) {
                    Some(p) => WifiAction::SendEap(p),
                    None => WifiAction::Error(ERROR_EAP),
                }
            }
            _ => WifiAction::Error(ERROR_EAP),
        }
    }

    /// 802.1X EAP-Success 后注入 MSK（EAP-TLS 等键派生方法），得到 PMK。
    pub fn on_eap_success(&mut self, msk: &[u8]) -> WifiAction {
        let n = msk.len().min(32);
        self.pmk[..n].copy_from_slice(&msk[..n]);
        self.phase = ConnectPhase::FourWay;
        self.state = dp::WIFI_STATE_HANDSHAKE;
        WifiAction::None
    }

    /// 主动断开。返回 Disconnected（若此前已连接）。
    pub fn disconnect(&mut self) -> WifiAction {
        self.state = dp::WIFI_STATE_IDLE;
        self.phase = ConnectPhase::Idle;
        self.handshake = None;
        self.sae = None;
        WifiAction::Disconnected
    }

    /// 收到去关联/去认证帧。
    pub fn on_disconnected(&mut self) -> WifiAction {
        self.state = dp::WIFI_STATE_IDLE;
        self.phase = ConnectPhase::Idle;
        self.handshake = None;
        self.sae = None;
        WifiAction::Disconnected
    }

    /// 更新当前 BSS 的 RSSI；连接状态下若低于阈值且存在更优 BSS → 漫游。
    pub fn update_rssi(&mut self, rssi: i8) -> RoamDecision {
        self.current_rssi = rssi;
        if self.state != dp::WIFI_STATE_CONNECTED {
            return RoamDecision::None;
        }
        // 刷新扫描表中当前 BSS 的信号强度（后台扫描得到的可能已过时）。
        for e in self.scan_table.iter_mut().take(self.scan_count) {
            if e.bssid == self.connected_bssid {
                e.rssi = rssi;
            }
        }
        if rssi >= RSSI_ROAM_THRESHOLD {
            return RoamDecision::None;
        }
        let best = match self.pick_best_bss(self.target_ssid_str()) {
            Some(b) => b,
            None => return RoamDecision::None,
        };
        if best.rssi > rssi + RSSI_BETTER_MARGIN && best.bssid != self.connected_bssid {
            return RoamDecision::Handover {
                bssid: best.bssid,
                channel: best.channel,
            };
        }
        RoamDecision::None
    }

    /// 漫游：切换到新 BSS 后重连（基础重连；802.11r FT 另行实现）。
    pub fn roam_to(&mut self, bssid: [u8; 6]) -> WifiAction {
        self.target_bssid = bssid;
        self.reconnect_attempts = self.reconnect_attempts.wrapping_add(1);
        self.state = dp::WIFI_STATE_CONNECTING;
        self.phase = ConnectPhase::Assoc;
        WifiAction::SendAssoc
    }

    pub fn state(&self) -> u8 {
        self.state
    }
}

/// 开始认证：开放/PSK/802.1X 走开放认证，SAE 走 SAE commit。
fn begin_auth(sta: &mut WifiSta, entry: ScanEntry) -> WifiAction {
    sta.state = dp::WIFI_STATE_CONNECTING;
    match entry.auth {
        dp::WIFI_AUTH_WPA3_SAE => {
            let own = sta.own_mac;
            sta.sae_begin(&own)
        }
        _ => {
            sta.phase = ConnectPhase::Auth;
            WifiAction::SendAuth(ie::AUTH_OPEN, 1, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eapol::FourWay;
    use crate::prf::Ptk;

    fn entry(ssid: &[u8], bssid: [u8; 6], ch: u8, rssi: i8, auth: u8) -> ScanEntry {
        ScanEntry::new(ssid, bssid, ch, rssi, auth, false)
    }

    #[test]
    fn scan_pick_best_bss() {
        let mut sta = WifiSta::new();
        let bss_a = [0x02, 0x00, 0x00, 0x00, 0x00, 0x11];
        let bss_b = [0x02, 0x00, 0x00, 0x00, 0x00, 0x22];
        sta.scan_result(entry(b"kunx", bss_a, 6, -70, dp::WIFI_AUTH_WPA2_PSK));
        sta.scan_result(entry(b"kunx", bss_b, 1, -55, dp::WIFI_AUTH_WPA2_PSK));
        sta.scan_result(entry(b"other", [0xaa; 6], 6, -40, dp::WIFI_AUTH_OPEN));
        assert_eq!(sta.scan_count, 3);
        let best = sta.pick_best_bss(b"kunx").unwrap();
        assert_eq!(best.bssid, bss_b);
        assert_eq!(best.channel, 1);
    }

    #[test]
    fn open_network_connect() {
        let mut sta = WifiSta::new();
        let bssid = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        sta.scan_result(entry(b"openap", bssid, 6, -60, dp::WIFI_AUTH_OPEN));
        let a = sta.connect(b"openap", dp::WIFI_AUTH_OPEN, Some(bssid));
        assert_eq!(a, WifiAction::SendAuth(ie::AUTH_OPEN, 1, 0));
        let a = sta.on_auth_result(ie::STATUS_SUCCESS);
        assert_eq!(a, WifiAction::SendAssoc);
        let a = sta.on_assoc_result(ie::STATUS_SUCCESS, ie::CAP_ESS);
        assert_eq!(a, WifiAction::Connected);
        assert_eq!(sta.state(), dp::WIFI_STATE_CONNECTED);
        assert_eq!(sta.connected_bssid, bssid);
    }

    #[test]
    fn wpa2_psk_four_way() {
        let mut sta = WifiSta::new();
        let ap_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let sta_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let pmk = crate::pbkdf2::pbkdf2_sha1(b"password", b"kunx", 4096, 32);
        let mut pmk32 = [0u8; 32];
        pmk32.copy_from_slice(&pmk[..32]);
        sta.pmk = pmk32;
        sta.target_ssid[..4].copy_from_slice(b"kunx");
        sta.target_ssid_len = 4;
        sta.target_bssid = ap_mac;
        sta.target_auth = dp::WIFI_AUTH_WPA2_PSK;
        sta.state = dp::WIFI_STATE_HANDSHAKE;
        sta.phase = ConnectPhase::FourWay;

        let mut ap = FourWay {
            pmk: pmk32,
            aa: ap_mac,
            sa: sta_mac,
            anonce: [0x11u8; 32],
            snonce: [0u8; 32],
            replay_counter: 1,
            ptk: Ptk::from_ptk(&[0u8; 64]),
            kdv: KDV_HMAC_SHA1,
        };
        let rsn = ie::build_rsn_ie(ie::CIPHER_CCMP, ie::AKM_PSK, 0);

        let mut f1 = [0u8; 200];
        let n1 = ap.ap_build_msg1(&mut f1).unwrap();
        let act = sta.on_eapol_key(&f1[..n1], &sta_mac);
        let msg2 = match act {
            WifiAction::SendEapol(p) => p.as_slice().to_vec(),
            other => panic!("expected SendEapol, got {other:?}"),
        };
        let m2 = EapolKey::parse(&msg2).unwrap();
        assert_eq!(eapol::classify(m2.key_info), HandshakeMsg::Msg2);

        ap.snonce = [0x33u8; 32];
        let ptk = crate::prf::derive_ptk(&pmk32, &ap_mac, &sta_mac, &ap.anonce, &ap.snonce);
        ap.ptk = Ptk::from_ptk(&ptk);
        assert!(ap.ap_verify_msg2(&m2, &msg2));
        let mut f3 = [0u8; 300];
        let n3 = ap.ap_build_msg3(&[0x44u8; 32], &rsn, &mut f3).unwrap();

        let act = sta.on_eapol_key(&f3[..n3], &sta_mac);
        match act {
            WifiAction::SendEapol(_) => {}
            other => panic!("expected SendEapol, got {other:?}"),
        }
        assert_eq!(sta.state(), dp::WIFI_STATE_CONNECTED);
        assert_eq!(sta.phase, ConnectPhase::Connected);
    }

    #[test]
    fn enterprise_eap_md5_flow() {
        let mut sta = WifiSta::new();
        sta.set_eap_credentials(b"user@kunxos", b"secret123");

        let mut ap = EapAuthenticator::new(b"secret123");
        let mut req = [0u8; 128];
        let n = eap::build_identity_response(1, b"user@kunxos", &mut req).unwrap();
        let id_pkt = EapPacket::parse(&req[..n]).unwrap();
        let mut out = [0u8; 320];
        let (_n, _done) = ap.handle(&id_pkt, &mut out);
        let md5_req = EapPacket::parse(&out).unwrap();
        assert_eq!(md5_req.etype, eap::EAP_TYPE_MD5);

        let act = sta.on_eap_request(&md5_req);
        let resp = match act {
            WifiAction::SendEap(p) => p.as_slice().to_vec(),
            other => panic!("expected SendEap, got {other:?}"),
        };
        let mut out2 = [0u8; 128];
        let (_n2, done) = ap.handle(&EapPacket::parse(&resp).unwrap(), &mut out2);
        assert!(done);
        assert_eq!(EapPacket::parse(&out2).unwrap().code, eap::EAP_SUCCESS);

        let msk = [0x55u8; 64];
        let a = sta.on_eap_success(&msk);
        assert_eq!(a, WifiAction::None);
        assert_eq!(sta.state, dp::WIFI_STATE_HANDSHAKE);
        assert_eq!(sta.phase, ConnectPhase::FourWay);
        let _ = out2;
    }

    #[test]
    fn wpa3_sae_via_manager() {
        let mut sta = WifiSta::new();
        sta.set_psk(b"password");
        let ap_bssid = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        sta.scan_result(entry(b"saenet", ap_bssid, 36, -65, dp::WIFI_AUTH_WPA3_SAE));
        let a = sta.connect(b"saenet", dp::WIFI_AUTH_WPA3_SAE, Some(ap_bssid));
        let commit_own = match a {
            WifiAction::SendSaeCommit(p) => p.as_slice().to_vec(),
            other => panic!("expected SendSaeCommit, got {other:?}"),
        };

        // AP 侧独立会话。
        let own_mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
        let (pwe_ap, _) = sae::derive_pwe(&own_mac, &ap_bssid, b"password").unwrap();
        let (mut ap_sess, commit_ap) = sae::make_commit(&[0x33u8; 32], &[0x44u8; 32], &pwe_ap);

        let a = sta.on_sae_commit(&commit_ap);
        let _confirm_sta = match a {
            WifiAction::SendSaeConfirm(p) => p.as_slice().to_vec(),
            other => panic!("expected SendSaeConfirm, got {other:?}"),
        };
        let (sc_sta, elem_sta) = sae::parse_commit(&commit_own).unwrap();
        assert!(sae::on_peer_commit(&mut ap_sess, &[0x33u8; 32], &sc_sta, &elem_sta));
        ap_sess.send_confirm = 1;
        let confirm_ap = sae::make_confirm(&ap_sess, &sc_sta, &elem_sta);

        let a = sta.on_sae_confirm(&confirm_ap, ap_sess.send_confirm);
        assert_eq!(a, WifiAction::SendAssoc);
        assert_eq!(sta.pmk, ap_sess.pmk);
        let _ = own_mac;
    }

    #[test]
    fn basic_roaming_decision() {
        let mut sta = WifiSta::new();
        let bss_a = [0x02, 0x00, 0x00, 0x00, 0x00, 0x11];
        let bss_b = [0x02, 0x00, 0x00, 0x00, 0x00, 0x22];
        sta.target_ssid[..4].copy_from_slice(b"kunx");
        sta.target_ssid_len = 4;
        sta.state = dp::WIFI_STATE_CONNECTED;
        sta.connected_bssid = bss_a;
        sta.current_rssi = -50;
        sta.scan_result(entry(b"kunx", bss_a, 6, -50, dp::WIFI_AUTH_WPA2_PSK));
        sta.scan_result(entry(b"kunx", bss_b, 1, -60, dp::WIFI_AUTH_WPA2_PSK));
        // 信号尚可 → 无需漫游
        assert_eq!(sta.update_rssi(-60), RoamDecision::None);
        // 信号很差且存在明显更优 BSS → 漫游
        let r = sta.update_rssi(-90);
        match r {
            RoamDecision::Handover { bssid, .. } => assert_eq!(bssid, bss_b),
            other => panic!("expected Handover, got {other:?}"),
        }
    }
}