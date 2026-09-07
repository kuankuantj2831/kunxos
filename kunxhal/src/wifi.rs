//! KunxOS Wi-Fi 硬件抽象层（HAL）：扫描 / 连接 / 断开 / 状态查询。
//!
//! 约束与设计：
//! - `no_std`、无堆、无 panic：所有方法返回 `Result`/`Option`/`bool`。
//! - 这是「网卡后端」级抽象，只描述 802.11 链路行为：
//!   - 扫描（主动扫描，返回 BSS 列表）
//!   - 关联（associate）/ 去关联（disassociate）
//!   - EAPOL 帧中继（`recv_eapol` / `send_eapol`，载荷为完整以太网帧，
//!     ethertype 0x888E）：WPA2 4-way handshake 由内核（kunxcore::wifi）
//!     在 Rust 侧完成（见 `kunxwireless::eapol`），本层只负责把帧收发给 AP。
//!   - 链路状态：`Idle → Associating → Associated → Secured(=4-way 完成)`
//!   - 租约：QEMU 模拟后端直接返回固定租约；真实后端返回 `false`，由内核
//!     执行线上 DHCP。
//!
//! 与平台后端（`plat_qemu` / `plat_pi4`）的关系：
//! - QEMU：`SimWifi` —— 软件模拟 AP + **真实的** STA/AP 双侧 4-way handshake
//!   （复用 `kunxwireless::eapol::FourWay`），因此 QEMU 下整条「扫描 →
//!   4-way → 已连接」链路是真实密码学可验证的（错误口令在 AP 侧 MIC 校验被拒）。
//! - Pi4：`CywWifi` —— CYW43455 SDIO 后端骨架（BCM2711 dwmmc @0xFE300000），
//!   含固件下载入口与 SDPCM/WLC ioctl 定义，硬件接线待真机验证。
//!
//! 字节布局与 `/dev/wifi` 协议（`kunxwireless::devproto`）保持一致：HAL 的
//! [`WifiScanResult`] 与协议中的 `WifiScanRecord` 字段一一对应。

use core::fmt;

/// SSID 最大长度（802.11 规范为 32 字节）。
pub const WIFI_SSID_MAX: usize = 32;
/// WPA2-PSK 口令最大长度（8..63 字节）。
pub const WIFI_PSK_MAX: usize = 64;
/// BSSID 长度（6 字节 MAC）。
pub const WIFI_BSSID_LEN: usize = 6;

/// 认证类型（与 `kunxwireless::devproto` 的 `WIFI_AUTH_*` 对应）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WifiAuth {
    /// 开放网络（无加密）。
    Open = 0,
    /// WPA2-PSK（初始范围只支持这一种握手）。
    Wpa2Psk = 1,
    /// WPA3-SAE（识别但不支持，任务约定后续再说）。
    Wpa3Sae = 2,
    /// 802.1X 企业认证（不支持）。
    Dot1x = 3,
    /// 其它 / 未知。
    Other,
}

/// 从协议字节构造认证类型。
pub fn wifi_auth_from_u8(v: u8) -> WifiAuth {
    match v {
        0 => WifiAuth::Open,
        1 => WifiAuth::Wpa2Psk,
        2 => WifiAuth::Wpa3Sae,
        3 => WifiAuth::Dot1x,
        _ => WifiAuth::Other,
    }
}

/// 一条扫描结果（BSS）。
#[derive(Clone, Copy, Debug)]
pub struct WifiScanResult {
    pub ssid: [u8; WIFI_SSID_MAX],
    pub ssid_len: u8,
    pub bssid: [u8; WIFI_BSSID_LEN],
    pub channel: u8,
    pub rssi: i8,
    pub auth: WifiAuth,
    /// 位 0：隐藏网络（SSID 为空，需主动探测才能关联）。
    pub hidden: bool,
}

impl WifiScanResult {
    pub const fn empty() -> Self {
        WifiScanResult {
            ssid: [0; WIFI_SSID_MAX],
            ssid_len: 0,
            bssid: [0; WIFI_BSSID_LEN],
            channel: 0,
            rssi: 0,
            auth: WifiAuth::Other,
            hidden: false,
        }
    }

    /// SSID 字符串视图。
    pub fn ssid_str(&self) -> &[u8] {
        &self.ssid[..self.ssid_len as usize]
    }
}

/// 链路安全状态（4-way handshake 完成前 AP 不让数据通过）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WifiLinkState {
    /// 无硬件 / 未初始化。
    Unavailable,
    /// 空闲（未关联）。
    Idle,
    /// 正在关联（发送 Assoc Request / 等待 Assoc Response）。
    Associating,
    /// 已关联（开放网络即可直接传输；WPA2 仍需 4-way）。
    Associated,
    /// 4-way handshake 完成，密钥已安装，可传输数据。
    Secured,
    /// 关联/握手失败（如口令错误、AP 拒绝）。
    Failed,
}

/// 当前状态快照（/dev/wifi 状态记录的上游）。
#[derive(Clone, Copy, Debug)]
pub struct WifiStatus {
    pub link: WifiLinkState,
    pub ssid: [u8; WIFI_SSID_MAX],
    pub ssid_len: u8,
    pub bssid: [u8; WIFI_BSSID_LEN],
    pub rssi: i8,
}

impl WifiStatus {
    pub const fn idle() -> Self {
        WifiStatus {
            link: WifiLinkState::Idle,
            ssid: [0; WIFI_SSID_MAX],
            ssid_len: 0,
            bssid: [0; WIFI_BSSID_LEN],
            rssi: 0,
        }
    }

    pub fn ssid_str(&self) -> &[u8] {
        &self.ssid[..self.ssid_len as usize]
    }
}

/// Wi-Fi HAL 错误。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WifiError {
    /// 无 Wi-Fi 硬件。
    Unavailable,
    /// 未找到对应的 AP（扫描结果中无此 SSID/BSSID）。
    NotFound,
    /// 设备忙（扫描中 / 关联中）。
    Busy,
    /// 操作超时。
    TimedOut,
    /// 认证失败（4-way handshake MIC 校验失败，通常口令错误）。
    AuthFailed,
    /// 不支持的认证类型（WPA3 / 802.1X / WEP）。
    Unsupported,
    /// 后端尚未接线（真正的硬件驱动未完成）。
    NotImplemented,
}

impl fmt::Display for WifiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            WifiError::Unavailable => "no wifi hardware",
            WifiError::NotFound => "ap not found",
            WifiError::Busy => "busy",
            WifiError::TimedOut => "timeout",
            WifiError::AuthFailed => "authentication failed (bad psk?)",
            WifiError::Unsupported => "unsupported auth",
            WifiError::NotImplemented => "not implemented",
        };
        f.write_str(s)
    }
}

/// `poll_scan` 的一次返回值：`count` 条新结果已写入 `out`，`done` 表示扫描完成。
#[derive(Clone, Copy, Debug)]
pub struct WifiScanPoll {
    pub done: bool,
    pub count: usize,
}

/// 以太网类型：EAPOL（802.1X 端口访问，4-way handshake 帧）。
pub const ETH_TYPE_EAPOL: u16 = 0x888E;

/// Wi-Fi 网卡后端抽象。
///
/// 实现分布在平台模块：`[super::plat_qemu]::SimWifi`（QEMU，可用）与
/// `[super::plat_pi4]::CywWifi`（Pi4，骨架）。内核 `kunxcore::wifi`
/// 通过「命令 → 事件」协议驱动本 trait（见 `kunxwireless::devproto`）。
pub trait WifiDevice {
    /// 后端名称（日志用）。
    fn name(&self) -> &'static str;
    /// 无线接口 MAC（STA 地址，4-way handshake 的 SA）。
    fn mac(&self) -> [u8; WIFI_BSSID_LEN];
    /// 硬件是否可用（寄存器探测 / 固件加载结果）。
    fn available(&self) -> bool;

    /// 启动一次主动扫描；立即返回，结果经 [`Self::poll_scan`] 逐步取走。
    fn start_scan(&mut self) -> bool;
    /// 取走扫描结果（每次调用的新结果写入 `out`）。
    fn poll_scan(&mut self, out: &mut [WifiScanResult]) -> WifiScanPoll;

    /// 关联指定 BSSID（`sta_mac` 为本机 STA 地址，供模拟后端构造帧头）。
    fn associate(&mut self, bssid: [u8; WIFI_BSSID_LEN], sta_mac: [u8; WIFI_BSSID_LEN])
        -> Result<(), WifiError>;
    /// 去关联（断开）。
    fn disassociate(&mut self);
    /// 当前链路状态。
    fn link_state(&self) -> WifiLinkState;

    /// 收一条 AP → STA 的 EAPOL 帧（完整以太网帧，ethertype 0x888E）。
    fn recv_eapol(&mut self, out: &mut [u8]) -> Option<usize>;
    /// 发一条 STA → AP 的 EAPOL 帧（完整以太网帧）。
    fn send_eapol(&mut self, frame: &[u8]) -> bool;

    /// 当前状态快照。
    fn status(&self) -> WifiStatus;

    /// 静态租约（QEMU 模拟后端直接返回）；真实后端返回 `false`，
    /// 由内核执行线上 DHCP 客户端。
    fn lease(&self, _ip: &mut u32, _gw: &mut u32, _mask: &mut u32, _dns: &mut u32) -> bool {
        false
    }
}