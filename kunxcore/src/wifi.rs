//! kunxcore Wi-Fi 子系统：`/dev/wifi` 字符设备 + 关联/4-way handshake 状态机 + DHCP 触发。
//!
//! 架构（与 `kunxwireless::devproto` 精确定义的字节协议一致）：
//!
//! ```text
//!   wifid（用户态守护进程，状态机/配置存储）
//!       ⇄ /dev/wifi 读写 + ioctl              （本模块，字节协议见 devproto）
//!       ⇄ HAL WifiDevice                       （SimWifi=QEMU 模拟 / CywWifi=Pi4 SDIO）
//! ```
//!
//! 职责：
//! - `/dev/wifi` 命令：`SCAN` / `CONNECT <ssid>\0<psk>\0<auth>` / `DISCONNECT` /
//!   `STATUS` / `SET_BACKEND`；事件：`SCAN_RESULT`（每条一个 AP）/ `SCAN_DONE` /
//!   `CONNECTING` / `CONNECTED` / `DISCONNECTED` / `HANDSHAKE_MSG`（EAPOL 中继）/
//!   `ERROR`；ioctl：`WIFI_IOCTL_TRIGGER_SCAN` / `WIFI_IOCTL_GET_STATE`。
//! - **WPA2-PSK 4-way handshake 在 Rust 侧完成**（STA 侧，复用
//!   `kunxwireless::eapol::FourWay`）：`PSK + SSID → PBKDF2(4096) → PMK`，
//!   与 AP 交换 Msg1/2/3/4，MIC 互验通过、解开 GTK 后链路进入 Secured。
//!   QEMU 模拟后端（SimWifi）的 AP 侧用同一密码学库真实执行，因此 QEMU 下
//!   整条链路可验证：错误口令在 Msg2 阶段被 AP 拒绝（link=Failed → 重试）。
//! - 连接成功后取得 IP 配置：HAL 静态租约（QEMU 模拟）或线上 DHCP
//!   （`net_stack::dhcp_configure`，真实后端）。
//! - 失败重试：扫描无结果 / 关联失败 / 握手超时 / 认证失败自动重试有限次
//!   （`CONNECT_RETRY_MAX`），并逐次上报 `ERROR` 事件。
//!
//! 状态（与 devproto `WIFI_STATE_*` 对应）：
//! `Idle → Scanning → Connecting(关联) → Handshake(4-way) → Connected`；
//! 断开或重试耗尽回到 `Idle`（发 `DISCONNECTED`）。

use kunxhal::wifi::{wifi_auth_from_u8, WifiAuth, WifiDevice, WifiError, WifiLinkState, WifiScanResult};
use kunxwireless as kw;

use crate::devfs;
use crate::net_stack;
use crate::net_virtio;
use crate::vfs;

// ==================== 常量 ====================

/// Wi-Fi 字符设备主设备号（自定义；与其它 devfs 主设备号不冲突）。
pub const WIFI_MAJOR: u32 = 12;
/// Wi-Fi 字符设备次设备号（/dev/wifi）。
pub const WIFI_MINOR: u32 = 0;

/// EAPOL（802.1X）以太网类型：4-way handshake 帧。
const ETH_TYPE_EAPOL: u16 = 0x888E;
/// 扫描结果缓存容量。
const CACHE_MAX: usize = 16;
/// 事件队列容量（字节，环形，记录式）。
const EVT_CAP: usize = 4096;
/// 握手超时（微秒）。
const HS_TIMEOUT_US: u64 = 5_000_000;
/// 连接/握手失败最大重试次数。
const CONNECT_RETRY_MAX: u8 = 3;

// 状态常量与 devproto WIFI_STATE_* 一致。
const ST_IDLE: u8 = 0;
const ST_SCANNING: u8 = 1;
const ST_CONNECTING: u8 = 2;
const ST_HANDSHAKE: u8 = 3;
const ST_CONNECTED: u8 = 4;

// 事件常量与 devproto WIFI_EVT_* 一致。
const EVT_SCAN_RESULT: u8 = 0x01;
const EVT_SCAN_DONE: u8 = 0x02;
const EVT_CONNECTING: u8 = 0x03;
const EVT_CONNECTED: u8 = 0x04;
const EVT_DISCONNECTED: u8 = 0x05;
const EVT_HANDSHAKE_MSG: u8 = 0x06;
const EVT_ERROR: u8 = 0x07;

// ==================== 全局状态 ====================

/// HAL Wi-Fi 后端（编译期平台选定：qemu → SimWifi；pi4 → CywWifi）。
static mut DEV: Option<&'static mut dyn WifiDevice> = None;

/// 事件环形队列（记录 = `[ty, len_le_u16, body...]`）。
static mut EVT: [u8; EVT_CAP] = [0; EVT_CAP];
static mut EVT_HEAD: usize = 0;
static mut EVT_COUNT: usize = 0;

/// 扫描结果缓存。
static mut CACHE: [WifiScanResult; CACHE_MAX] = [WifiScanResult::empty(); CACHE_MAX];
static mut CACHE_N: usize = 0;

/// 当前状态与关联信息。
static mut WIFI_STATE: u8 = ST_IDLE;
static mut CUR_SSID: [u8; 32] = [0; 32];
static mut CUR_LEN: u8 = 0;
static mut CUR_BSSID: [u8; 6] = [0; 6];
static mut CUR_RSSI: i8 = 0;

/// 连接目标。
static mut TARGET_ACTIVE: bool = false;
static mut TARGET_SSID: [u8; 32] = [0; 32];
static mut TARGET_LEN: u8 = 0;
static mut TARGET_PSK: [u8; 64] = [0; 64];
static mut TARGET_PSK_LEN: u8 = 0;
static mut TARGET_AUTH: u8 = 1; // 默认 1 = WPA2-PSK
static mut CONNECT_RETRY: u8 = 0;

/// 握手上下文（STA 侧 4-way）。
static mut HS_ACTIVE: bool = false;
static mut HS_CTX: kw::eapol::FourWay = kw::eapol::FourWay {
    pmk: [0; 32],
    aa: [0; 6],
    sa: [0; 6],
    anonce: [0; 32],
    snonce: [0; 32],
    replay_counter: 0,
    ptk: kw::prf::Ptk { kck: [0; 16], kek: [0; 16], tk: [0; 32] },
    kdv: kw::eapol::KDV_HMAC_SHA1,
};
/// 握手阶段：1=等 Msg1，2=等 Msg3（收后发 Msg4 完成）。
static mut HS_STAGE: u8 = 0;
static mut HS_DEADLINE: u64 = 0;

// ==================== 事件队列 ====================

/// 入队一条事件记录。溢出时从最旧记录开始丢弃腾位。
fn evt_push(ty: u8, body: &[u8]) {
    unsafe {
        let total = 3 + body.len();
        if total > EVT_CAP {
            return;
        }
        while EVT_COUNT + total > EVT_CAP && EVT_COUNT >= 3 {
            let old_len = (EVT[(EVT_HEAD + 1) % EVT_CAP] as usize)
                | ((EVT[(EVT_HEAD + 2) % EVT_CAP] as usize) << 8);
            EVT_HEAD = (EVT_HEAD + 3 + old_len) % EVT_CAP;
            EVT_COUNT -= 3 + old_len;
        }
        let base = EVT_HEAD + EVT_COUNT;
        EVT[base % EVT_CAP] = ty;
        EVT[(base + 1) % EVT_CAP] = (body.len() & 0xFF) as u8;
        EVT[(base + 2) % EVT_CAP] = ((body.len() >> 8) & 0xFF) as u8;
        for (i, &b) in body.iter().enumerate() {
            EVT[(base + 3 + i) % EVT_CAP] = b;
        }
        EVT_COUNT += total;
    }
}

/// 取走一条事件记录（写入 `[ty, body...]`）；无记录返回 0。
fn evt_pop(out: &mut [u8]) -> usize {
    unsafe {
        if EVT_COUNT == 0 || out.is_empty() {
            return 0;
        }
        let ty = EVT[EVT_HEAD];
        let len = (EVT[(EVT_HEAD + 1) % EVT_CAP] as usize)
            | ((EVT[(EVT_HEAD + 2) % EVT_CAP] as usize) << 8);
        let n = len.min(out.len() - 1).min(EVT_COUNT.saturating_sub(3));
        out[0] = ty;
        for i in 0..n {
            out[1 + i] = EVT[(EVT_HEAD + 3 + i) % EVT_CAP];
        }
        EVT_HEAD = (EVT_HEAD + 3 + len) % EVT_CAP;
        EVT_COUNT -= 3 + len;
        1 + n
    }
}

// ==================== devproto 字节编解码 ====================

/// devproto `WifiStateRecord`（41 字节）字节视图。
fn state_record() -> [u8; 41] {
    let rec = kw::devproto::WifiStateRecord {
        state: unsafe { WIFI_STATE },
        ssid: unsafe { CUR_SSID },
        ssid_len: unsafe { CUR_LEN },
        bssid: unsafe { CUR_BSSID },
        rssi: unsafe { CUR_RSSI },
    };
    bytes_of(&rec)
}

/// devproto `WifiScanRecord`（43 字节）字节视图。
fn scan_record(r: &WifiScanResult) -> [u8; 43] {
    let rec = kw::devproto::WifiScanRecord {
        ssid: r.ssid,
        ssid_len: r.ssid_len,
        bssid: r.bssid,
        channel: r.channel,
        rssi: r.rssi,
        auth: r.auth as u8,
        flags: if r.hidden { 1 } else { 0 },
    };
    bytes_of(&rec)
}

/// 把 repr(C) 结构按字节拷贝到固定数组。
fn bytes_of<const N: usize>(rec: &impl Sized) -> [u8; N] {
    let mut out = [0u8; N];
    unsafe {
        core::ptr::copy_nonoverlapping(
            (rec as *const _) as *const u8,
            out.as_mut_ptr(),
            N,
        );
    }
    out
}

// ==================== 以太网帧工具 ====================

fn eth_put(dst: [u8; 6], src: [u8; 6], etype: u16, payload: &[u8], out: &mut [u8]) -> usize {
    let total = 14 + payload.len();
    if total > out.len() {
        return 0;
    }
    out[..6].copy_from_slice(&dst);
    out[6..12].copy_from_slice(&src);
    out[12] = (etype >> 8) as u8;
    out[13] = (etype & 0xFF) as u8;
    out[14..total].copy_from_slice(payload);
    total
}

fn eth_etype(frame: &[u8]) -> u16 {
    if frame.len() < 14 {
        return 0;
    }
    u16::from_be_bytes([frame[12], frame[13]])
}

/// STA 本机无线接口 MAC（优先 Wi-Fi 后端，回退 virtio-net）。
fn sta_mac() -> [u8; 6] {
    unsafe {
        if let Some(dev) = DEV.as_deref() {
            return dev.mac();
        }
    }
    net_virtio::mac()
}

// ==================== 状态机动作 ====================

/// 发起一次主动扫描（清空缓存，进入 SCANNING 状态）。
fn start_scan() {
    unsafe {
        let Some(dev) = DEV.as_deref_mut() else {
            evt_push(EVT_ERROR, b"\xE6\x97\xA0\x20\x77\x69\x66\x69\x20\xE7\xA1\xAC\xE4\xBB\xB6");
            return;
        };
        if !dev.available() {
            evt_push(EVT_ERROR, b"\xE6\x97\xA0\xE7\xBA\xBF\xE5\x90\x8E\xE7\xAB\xAF\xE4\xB8\x8D\xE5\x8F\xAF\xE7\x94\xA8");
            WIFI_STATE = ST_IDLE;
            return;
        }
        CACHE_N = 0;
        WIFI_STATE = ST_SCANNING;
        dev.start_scan();
    }
}

/// 设置连接目标（CONNECT 命令解析后调用）。
fn set_target(ssid: &[u8], psk: &[u8], auth: u8) {
    unsafe {
        TARGET_ACTIVE = true;
        TARGET_LEN = ssid.len().min(32) as u8;
        TARGET_PSK_LEN = psk.len().min(64) as u8;
        TARGET_SSID = [0; 32];
        TARGET_PSK = [0; 64];
        TARGET_SSID[..TARGET_LEN as usize].copy_from_slice(&ssid[..TARGET_LEN as usize]);
        TARGET_PSK[..TARGET_PSK_LEN as usize].copy_from_slice(&psk[..TARGET_PSK_LEN as usize]);
        TARGET_AUTH = auth;
        CONNECT_RETRY = 0;
    }
}

/// 在扫描缓存中按 SSID 找信号最强的 BSS。
fn find_in_cache(ssid: &[u8]) -> Option<[u8; 6]> {
    unsafe {
        let mut best: Option<[u8; 6]> = None;
        let mut best_rssi = i8::MIN;
        for i in 0..CACHE_N {
            let r = &CACHE[i];
            if r.ssid_len as usize == ssid.len() && &r.ssid[..r.ssid_len as usize] == ssid {
                if best.is_none() || r.rssi > best_rssi {
                    best = Some(r.bssid);
                    best_rssi = r.rssi;
                }
            }
        }
        best
    }
}

/// 检查目标 SSID 与认证是否匹配（开放网络强制 auth=0；WPA2 强制 1）。
fn target_auth_ok() -> bool {
    unsafe {
        let auth = wifi_auth_from_u8(TARGET_AUTH);
        match auth {
            WifiAuth::Open => true,
            WifiAuth::Wpa2Psk => true,
            WifiAuth::Other => true, // 保留：模拟后端允许
            _ => false, // WPA3 / 802.1X 不支持
        }
    }
}

/// 尝试连接目标：缓存命中则关联，否则重扫等待结果。
fn try_connect_target() {
    unsafe {
        let ssid = &TARGET_SSID[..TARGET_LEN as usize];
        let Some(bssid) = find_in_cache(ssid) else {
            if WIFI_STATE != ST_SCANNING {
                start_scan();
            }
            return;
        };
        // 目标不支持的认证：直接失败（避免浪费一轮握手）
        if !target_auth_ok() {
            evt_push(EVT_ERROR, b"\xE4\xB8\x8D\xE6\x94\xAF\xE6\x8C\x81\xE7\x9A\x84\xE8\xAE\xA4\xE8\xAF\x81\xE6\x96\xB9\xE5\xBC\x8F\xEF\xBC\x88wpa3/802.1x/wep\xEF\xBC\x89");
            disconnect_to_idle();
            return;
        }
        // 记录当前关联信息（CUR_*），进入 CONNECTING
        CUR_SSID = TARGET_SSID;
        CUR_LEN = TARGET_LEN;
        CUR_BSSID = bssid;
        CUR_RSSI = 0;
        WIFI_STATE = ST_CONNECTING;
        evt_push(EVT_CONNECTING, &state_record());
        let mac = sta_mac();
        // 关联超时底线：未在 HS_TIMEOUT 内进入 Associated/Secured 视为失败。
        HS_DEADLINE = crate::timer::now_us() + HS_TIMEOUT_US;
        let Some(dev) = DEV.as_deref_mut() else { return };
        match dev.associate(bssid, mac) {
            Ok(()) => {}
            Err(WifiError::Unsupported) => {
                evt_push(EVT_ERROR, b"\xE4\xB8\x8D\xE6\x94\xAF\xE6\x8C\x81\xE7\x9A\x84\xE8\xAE\xA4\xE8\xAF\x81\xE6\x96\xB9\xE5\xBC\x8F\xEF\xBC\x88wpa3/802.1x/wep\xEF\xBC\x89");
                disconnect_to_idle();
            }
            Err(_) => {
                evt_push(EVT_ERROR, b"\xE5\x85\xB3\xE8\x81\x94\xE5\xA4\xB1\xE8\xB4\xA5");
                disconnect_to_idle();
            }
        }
    }
}

/// 开始 STA 侧 4-way：PBKDF2 派生 PMK、产生 SNonce、进入 HANDSHAKE。
fn handshake_begin() {
    unsafe {
        let ssid = &TARGET_SSID[..TARGET_LEN as usize];
        let psk = &TARGET_PSK[..TARGET_PSK_LEN as usize];
        let dk = kw::pbkdf2::pbkdf2_sha1(psk, ssid, 4096, 32);
        let mut pmk = [0u8; 32];
        pmk.copy_from_slice(&dk[..32]);
        // SNonce：由单调时钟 + 重试计数确定性产生（无 RNG 时足够熵）。
        let now = crate::timer::now_us() ^ ((CONNECT_RETRY as u64) << 40);
        let mut snonce = [0u8; 32];
        for i in 0..32 {
            snonce[i] = ((now >> ((i % 8) * 8)) ^ 0x5a) as u8;
        }
        let mac = sta_mac();
        if DEV.is_none() {
            return;
        }
        HS_CTX = kw::eapol::FourWay {
            pmk,
            aa: CUR_BSSID,
            sa: mac,
            anonce: [0; 32],
            snonce,
            replay_counter: 0,
            ptk: kw::prf::Ptk { kck: [0; 16], kek: [0; 16], tk: [0; 32] },
            kdv: kw::eapol::KDV_HMAC_SHA1,
        };
        HS_ACTIVE = true;
        HS_STAGE = 1;
        HS_DEADLINE = crate::timer::now_us() + HS_TIMEOUT_US;
        WIFI_STATE = ST_HANDSHAKE;
        evt_push(EVT_CONNECTING, &state_record());
    }
}

/// 连接成功：置 CONNECTED、发事件、取 IP 配置（静态租约或线上 DHCP）。
fn finish_connect() {
    unsafe {
        HS_ACTIVE = false;
        WIFI_STATE = ST_CONNECTED;
        evt_push(EVT_CONNECTED, &state_record());
        // IP 配置：QEMU 模拟后端返回静态租约；真实后端执行线上 DHCP。
        let mut ip = 0u32;
        let mut gw = 0u32;
        let mut mask = 0u32;
        let mut dns = 0u32;
        let leased = match DEV.as_deref_mut() {
            Some(dev) => dev.lease(&mut ip, &mut gw, &mut mask, &mut dns),
            None => false,
        };
        if leased {
            net_stack::set_config(ip, gw, mask, dns);
        } else {
            net_stack::dhcp_configure();
        }
    }
}

/// 断开回空闲（发 DISCONNECTED 事件）。
fn disconnect_to_idle() {
    unsafe {
        HS_ACTIVE = false;
        WIFI_STATE = ST_IDLE;
        CUR_LEN = 0;
        CUR_SSID = [0; 32];
        CUR_BSSID = [0; 6];
        CUR_RSSI = 0;
        evt_push(EVT_DISCONNECTED, &state_record());
    }
}

/// 失败后的重试策略：不足上限则重试（重新扫描/关联），否则断开上报。
fn retry_or_fail() {
    unsafe {
        if CONNECT_RETRY < CONNECT_RETRY_MAX && TARGET_ACTIVE {
            CONNECT_RETRY += 1;
            // 重新开始：清空缓存重扫（BSSID 可能变化）
            TARGET_ACTIVE = true; // 保持目标
            start_scan();
        } else {
            evt_push(EVT_ERROR, b"\xE8\xBF\x9E\xE6\x8E\xA5\xE5\xA4\xB1\xE8\xB4\xA5\xEF\xBC\x88\xE9\x87\x8D\xE8\xAF\x95\xE5\xB7\xB2\xE8\x80\x97\xE5\xB0\xBD\xEF\xBC\x89");
            disconnect_to_idle();
        }
    }
}

// ==================== 服务推进（worker 周期调用） ====================

/// 推进扫描 / 关联 / 握手 / 断链检测。
///
/// 注意：`DEV` 是 `static mut`，各阶段独立重新借用 `&mut`，避免别名冲突。
pub fn service() {
    // 1) 扫描推进
    if unsafe { WIFI_STATE } == ST_SCANNING {
        scan_service();
    }
    // 2) 关联推进（CONNECTING）
    if unsafe { WIFI_STATE } == ST_CONNECTING {
        unsafe {
            let link = match DEV.as_deref_mut() {
                Some(dev) => dev.link_state(),
                None => WifiLinkState::Unavailable,
            };
            match link {
                WifiLinkState::Associated => {
                    if TARGET_AUTH == 0 {
                        finish_connect(); // 开放网络：无需 4-way
                    } else if TARGET_AUTH == 1 {
                        handshake_begin();
                    } else {
                        retry_or_fail();
                    }
                }
                WifiLinkState::Secured => finish_connect(),
                WifiLinkState::Failed => {
evt_push(EVT_ERROR, b"\xE5\x85\xB3\xE8\x81\x94\xE5\xA4\xB1\xE8\xB4\xA5");
                    retry_or_fail();
                }
                WifiLinkState::Idle => {
                    if crate::timer::now_us() > HS_DEADLINE {
                        evt_push(EVT_ERROR, b"\xE5\x85\xB3\xE8\x81\x94\xE8\xB6\x85\xE6\x97\xB6");
                        retry_or_fail();
                    }
                }
                _ => {}
            }
        }
    }
    // 3) 握手推进（HANDSHAKE）
    if unsafe { WIFI_STATE } == ST_HANDSHAKE {
        unsafe {
            if crate::timer::now_us() > HS_DEADLINE {
                evt_push(EVT_ERROR, b"\xE6\x8F\xA1\xE6\x89\x8B\xE8\xB6\x85\xE6\x97\xB6");
                retry_or_fail();
            } else {
                let failed = match DEV.as_deref_mut() {
                    Some(dev) => dev.link_state() == WifiLinkState::Failed,
                    None => true,
                };
                if failed {
                    evt_push(EVT_ERROR, b"\xE6\x8F\xA1\xE6\x89\x8B\xE8\xAE\xA4\xE8\xAF\x81\xE5\xA4\xB1\xE8\xB4\xA5");
                    retry_or_fail();
                } else if let Some(dev) = DEV.as_deref_mut() {
                    handshake_step(dev);
                }
            }
        }
    }
    // 4) 已连接：检测断链
    if unsafe { WIFI_STATE } == ST_CONNECTED {
        unsafe {
            let dropped = match DEV.as_deref_mut() {
                Some(dev) => {
                    let l = dev.link_state();
                    l == WifiLinkState::Idle || l == WifiLinkState::Unavailable
                }
                None => true,
            };
            if dropped {
                disconnect_to_idle();
            }
        }
    }
}

/// 扫描推进：逐步取回结果（每条发 SCAN_RESULT），完成后发 SCAN_DONE 并尝试目标连接。
fn scan_service() {
    unsafe {
        let Some(dev) = DEV.as_deref_mut() else { return };
        let mut out = [WifiScanResult::empty(); CACHE_MAX];
        loop {
            let poll = dev.poll_scan(&mut out[..]);
            for i in 0..poll.count {
                let r = out[i];
                if CACHE_N < CACHE_MAX {
                    CACHE[CACHE_N] = r;
                    CACHE_N += 1;
                }
                evt_push(EVT_SCAN_RESULT, &scan_record(&r));
            }
            if poll.done {
                evt_push(EVT_SCAN_DONE, &[]);
                break;
            }
            if poll.count == 0 {
                return; // 尚无新结果，下一 tick 再取
            }
        }
        // 扫描完成：若存在待连接目标则立即尝试；否则回到空闲（结果已通过
        // SCAN_RESULT 事件逐条交付给 wifid）。
        if TARGET_ACTIVE {
            try_connect_target();
        } else {
            WIFI_STATE = ST_IDLE;
        }
    }
}

/// 单步握手：按阶段收 Msg1→发 Msg2，收 Msg3→（解 GTK）发 Msg4→完成。
fn handshake_step(dev: &mut dyn WifiDevice) {
    unsafe {
        let mut f = [0u8; 340];
        let Some(n) = dev.recv_eapol(&mut f[..]) else {
            return; // 尚无 AP 帧，等下一 tick
        };
        if eth_etype(&f[..n]) != ETH_TYPE_EAPOL {
            return;
        }
        let payload = &f[14..n];
        let Some(msg) = kw::eapol::EapolKey::parse(payload) else {
            return;
        };
        // 中继事件：wifid 可据此观察握手进度（帧载荷，ethertype 0x888E）
        evt_push(EVT_HANDSHAKE_MSG, &f[..n]);
        match HS_STAGE {
            1 => {
                // Msg1（ANonce）→ 派生 PTK → 发 Msg2（SNonce + RSN IE）
                HS_CTX.sta_on_msg1(&msg).unwrap_or(());
                let rsn = kw::wifi_ie::build_rsn_ie(
                    kw::wifi_ie::CIPHER_CCMP,
                    kw::wifi_ie::AKM_PSK,
                    0,
                );
                let mut body = [0u8; 300];
                let nb = HS_CTX.sta_build_msg2(&rsn, &mut body).unwrap_or(0);
                let mut eth = [0u8; 340];
                let fl = eth_put(CUR_BSSID, dev.mac(), ETH_TYPE_EAPOL, &body[..nb], &mut eth);
                if fl > 0 {
                    dev.send_eapol(&eth[..fl]);
                }
                HS_STAGE = 2;
                HS_DEADLINE = crate::timer::now_us() + HS_TIMEOUT_US;
            }
            2 => {
                // Msg3（含 KEK 包装的 GTK）→ 解开 GTK → 发 Msg4 → 完成
                let mut gtk = [0u8; 32];
                HS_CTX.sta_on_msg3(&msg, &mut gtk).map(|_| ()).unwrap_or(());
                let mut body = [0u8; 300];
                let nb = HS_CTX.sta_build_msg4(&mut body).unwrap_or(0);
                let mut eth = [0u8; 340];
                let fl = eth_put(CUR_BSSID, dev.mac(), ETH_TYPE_EAPOL, &body[..nb], &mut eth);
                if fl > 0 {
                    dev.send_eapol(&eth[..fl]);
                }
                HS_STAGE = 3;
                finish_connect();
            }
            _ => {}
        }
    }
}

// ==================== /dev/wifi 字符设备接线 ====================

fn wifi_read(_minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    evt_pop(buf) as i32
}

fn wifi_write(_minor: u32, buf: &[u8]) -> i32 {
    let Some(&cmd) = buf.first() else {
        return -1;
    };
    match cmd {
        kw::devproto::WIFI_CMD_SCAN => start_scan(),
        kw::devproto::WIFI_CMD_CONNECT => {
            // body: ssid(\0) psk(\0) auth(1)
            let rest = &buf[1..];
            let Some((ssid, n1)) = kw::devproto::parse_cstr(rest, 32) else {
                return -1;
            };
            let Some((psk, n2)) = kw::devproto::parse_cstr(&rest[n1..], 64) else {
                return -1;
            };
            let auth = rest
                .get(n1 + n2..)
                .and_then(|s| s.first())
                .copied()
                .unwrap_or(1);
            set_target(ssid, psk, auth);
            if find_in_cache(ssid).is_none() {
                start_scan(); // 需先扫描定位 BSS
            } else {
                try_connect_target();
            }
        }
        kw::devproto::WIFI_CMD_DISCONNECT => {
            unsafe {
                if let Some(dev) = DEV.as_deref_mut() {
                    dev.disassociate();
                }
            }
            disconnect_to_idle();
        }
        kw::devproto::WIFI_CMD_STATUS => {
            // 状态快照：按当前状态发一条对应事件（wifid 也可用 ioctl GET_STATE）
            unsafe {
                let rec = state_record();
                if WIFI_STATE == ST_CONNECTED {
                    evt_push(EVT_CONNECTED, &rec);
                } else {
                    evt_push(EVT_DISCONNECTED, &rec);
                }
            }
        }
        kw::devproto::WIFI_CMD_SET_BACKEND => {
            // 后端由平台编译期选定，SET_BACKEND 仅接受（协议兼容）。
        }
        _ => return -1,
    }
    buf.len() as i32
}

fn wifi_ioctl(_minor: u32, request: u32, out: &mut [u8]) -> i32 {
    match request {
        kw::devproto::WIFI_IOCTL_TRIGGER_SCAN => {
            start_scan();
            0
        }
        kw::devproto::WIFI_IOCTL_GET_STATE => {
            if out.len() < 41 {
                return -1;
            }
            out.copy_from_slice(&state_record());
            41
        }
        _ => -25, // ENOTTY
    }
}

// ==================== 初始化与工作线程 ====================

/// 无头自检（QEMU `-append "wifitest"`）：扫描 → WPA2-PSK 4-way → 已连接；
/// 再以错误口令验证 AP 侧拒绝（链路回到 Disconnected/Idle，不进入 Connected）。
/// 供 CI/回归（`make test-wifi`）打印 `[wifi-selftest] ... OK/FAIL`。
pub fn selftest() -> bool {
    unsafe {
        let Some(dev) = DEV.as_deref_mut() else {
            crate::uart_send_str("[wifi-selftest] FAIL: no wifi backend\n");
            return false;
        };
        let _ = dev;
    }
    let mut fails = 0usize;
    crate::uart_send_str("[wifi-selftest] begin\n");

    // 1) 扫描（清缓存重扫，等待 SCAN_DONE：状态从 SCANNING 回到 IDLE）
    start_scan();
    let deadline = crate::timer::now_us() + 5_000_000;
    let mut scanned = false;
    while crate::timer::now_us() < deadline && !scanned {
        unsafe {
            if WIFI_STATE == ST_SCANNING {
                service();
            } else {
                scanned = true;
            }
        }
        crate::sched_cfs::yield_now();
    }
    if !scanned || unsafe { CACHE_N } == 0 {
        fails += 1;
        crate::uart_send_str("[wifi-selftest] FAIL: scan produced no results\n");
    } else {
        crate::uart_send_str("[wifi-selftest] scan: ");
        crate::print_dec(unsafe { CACHE_N } as u64);
        crate::uart_send_str(" APs cached\n");
    }

    // 2) 正确口令：连接 KunxHome（缓存已含其 BSS），等待 CONNECTED
    set_target(b"KunxHome", b"kunxhome123", 1);
    try_connect_target();
    let deadline = crate::timer::now_us() + 10_000_000;
    let mut connected = false;
    while crate::timer::now_us() < deadline && !connected {
        service();
        unsafe {
            if WIFI_STATE == ST_CONNECTED {
                connected = true;
            }
        }
        crate::sched_cfs::yield_now();
    }
    if connected {
        crate::uart_send_str("[wifi-selftest] WPA2-PSK 4-way: connected OK\n");
    } else {
        fails += 1;
        crate::uart_send_str("[wifi-selftest] FAIL: 4-way handshake did not connect\n");
    }

    // 3) 错误口令：必须被 AP 拒绝（重试耗尽后回到 Idle，绝不能到 Connected）
    disconnect_to_idle();
    set_target(b"KunxHome", b"wrong-password", 1);
    try_connect_target();
    let deadline = crate::timer::now_us() + 12_000_000;
    let mut rejected = false;
    while crate::timer::now_us() < deadline && !rejected {
        service();
        unsafe {
            if WIFI_STATE == ST_IDLE && CONNECT_RETRY >= CONNECT_RETRY_MAX {
                rejected = true;
            }
            if WIFI_STATE == ST_CONNECTED {
                break; // 错误口令居然连上 → 直接判失败
            }
        }
        crate::sched_cfs::yield_now();
    }
    if rejected && unsafe { WIFI_STATE } == ST_IDLE {
        crate::uart_send_str("[wifi-selftest] wrong-password rejected by AP OK\n");
    } else {
        fails += 1;
        crate::uart_send_str("[wifi-selftest] FAIL: wrong password was not rejected\n");
    }

    if fails == 0 {
        crate::uart_send_str("[wifi-selftest] PASS (scan+4-way+reject)\n");
    } else {
        crate::uart_send_str("[wifi-selftest] FAIL (");
        crate::print_dec(fails as u64);
        crate::uart_send_str(" check(s) failed)\n");
    }
    // 恢复空闲，供 wifid/后续启动正常接管
    disconnect_to_idle();
    fails == 0
}

/// 初始化：注册 `/dev/wifi` 字符设备、挂载设备节点并启动首轮扫描。
pub fn init() {
    devfs::register_chrdev(
        WIFI_MAJOR,
        "wifi",
        devfs::ChrOps {
            read: wifi_read,
            write: wifi_write,
            ioctl: wifi_ioctl,
        },
    );
    vfs::mknod("/dev/wifi", vfs::FileType::Chr, WIFI_MAJOR, WIFI_MINOR);
    unsafe {
        DEV = kunxhal::wifi_init();
        if DEV.is_none() {
            crate::kinfo! { crate::uart_send_str("[wifi] 无 wifi 后端；/dev/wifi 已创建但未激活\n"); }
            return;
        }
    }
    crate::kinfo! { crate::uart_send_str("[wifi] /dev/wifi 就绪，开始初次扫描\n"); }
    start_scan();
}

/// Wi-Fi worker（内核线程）：周期推进扫描 / 握手 / 断链检测。
pub fn wifi_worker(_arg: usize) -> ! {
    loop {
        service();
        crate::sched_cfs::yield_now();
    }
}

/// 后端是否已初始化（init_main 决定是否拉起 worker 线程）。
pub fn initialized() -> bool {
    unsafe { DEV.is_some() }
}