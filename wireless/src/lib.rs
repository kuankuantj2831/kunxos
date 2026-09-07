//! KunxOS 无线协议栈共享库。
//!
//! 约束：`#![no_std]`、无堆分配、无 panic（所有函数要么纯计算要么返回
//! `Result`/`Option`），供内核（kunxcore）、系统守护进程（wifid / btd /
//! locationd）与宿主单元测试（`cargo test`）三处 `#[path]`-free 复用。
//!
//! 模块划分：
//! - 密码学原语：`aes` `sha1` `sha2` `hmac` `pbkdf2` `cmac` `keywrap` `prf` `p256`
//! - Wi-Fi 管理：`wifi_ie`（802.11 管理帧与信息元素）`wifi_mgmt`（扫描/连接/
//!   断开/基础漫游状态机）`wifi_config`（已知网络配置 + KeyStore 静态存储）
//!   `eapol`（WPA 4-way handshake）`eap`（802.1X EAP）`sae`（WPA3-SAE）
//! - 蓝牙：`hci`（HCI 命令/事件/ACL 编解码）`l2cap` `att` `gatt` `smp`（配对与
//!   Bonding）`rfcomm`（RFCOMM 复用器）`hfp`（HFP AT 命令与 eSCO）`a2dp`（AVDTP
//!   +SBC 音频）`brcm`（CYW43455 SDIO 传输与固件/NVRAM 下载）
//! - 定位：`nmea`（NMEA 0183 解析）`loc`（融合定位）
//! - 设备协议：`devproto`（/dev/wifi /dev/hci /dev/gnss 内核↔用户协议）

#![no_std]
#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]

#[cfg(test)]
extern crate std;

pub mod aes;
pub mod att;
pub mod cmac;
pub mod devproto;
pub mod eap;
pub mod eapol;
pub mod gatt;
pub mod hci;
pub mod hmac;
pub mod keywrap;
pub mod l2cap;
pub mod md5;
pub mod loc;
pub mod nmea;
pub mod p256;
pub mod pbkdf2;
pub mod prf;
pub mod sae;
pub mod sha1;
pub mod sha2;
pub mod a2dp;
pub mod brcm;
pub mod hfp;
pub mod rfcomm;
pub mod smp;
pub mod wifi_config;
pub mod wifi_ie;
pub mod wifi_mgmt;
