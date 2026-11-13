//! KunxOS 硬件抽象层（HAL）。
//!
//! 把各平台（QEMU virt / Raspberry Pi 4）的设备地址、中断号、时钟频率等
//! 硬编码差异收敛到统一的 [`Platform`] trait，内核与系统服务只依赖该接口，
//! 从而让同一套系统代码在两套平台上都能编译运行。
//!
//! 平台选择在编译期由 Cargo feature 决定：
//!   - `qemu`（默认）：QEMU `virt` 机器
//!   - `pi4`：Raspberry Pi 4（BCM2711）
//!
//! `make` 构建默认 QEMU，`make pi4` 传入 `--no-default-features --features pi4`。

#![no_std]

#[cfg(test)]
extern crate std;

pub mod audio;
pub mod camera;
pub mod display;
pub mod fusion;
pub mod gps;
pub mod input;
pub mod math;
pub mod motion;
pub mod net;
pub mod plat;
pub mod power;
pub mod sensor;
pub mod storage;
pub mod wifi;

#[cfg(feature = "qemu")]
mod plat_qemu;
#[cfg(feature = "pi4")]
mod plat_pi4;

// 相机后端窄接口：平台模块保持私有，仅导出相机所需的注入入口。
#[cfg(feature = "qemu")]
pub use plat_qemu::sim_camera_set_clock;
#[cfg(feature = "qemu")]
pub use plat_qemu::qemu_power_set_hooks;

pub use display::DisplayInfo;
pub use fusion::{Euler, Quat, RotationVector, Vec3};
pub use input::InputInfo;
pub use net::NetInfo;
pub use plat::Platform;
pub use sensor::{SensorConfig, SensorDevice, SensorInfo, SensorSample, SensorSource};
pub use storage::StorageInfo;
pub use wifi::{
    WifiAuth, WifiDevice, WifiError, WifiLinkState, WifiScanPoll, WifiScanResult, WifiStatus,
};

/// 返回当前平台的实现（由 Cargo feature 在编译期选定）。
///
/// - `qemu`（默认）：返回 QEMU virt 平台后端。
/// - `pi4`：返回 Raspberry Pi 4 平台后端。
#[cfg(feature = "pi4")]
pub fn current_platform() -> &'static dyn Platform {
    &plat_pi4::PI4
}

#[cfg(all(feature = "qemu", not(feature = "pi4")))]
pub fn current_platform() -> &'static dyn Platform {
    &plat_qemu::QEMU
}

/// 便捷入口：等价于 `current_platform().display_init()`。
pub fn display_init() -> Option<DisplayInfo> {
    current_platform().display_init()
}

/// 便捷入口：等价于 `current_platform().net_init()`。
pub fn net_init() -> Option<NetInfo> {
    current_platform().net_init()
}

/// 便捷入口：等价于 `current_platform().storage_init()`。
pub fn storage_init() -> Option<StorageInfo> {
    current_platform().storage_init()
}

/// 便捷入口：等价于 `current_platform().input_init()`。
pub fn input_init() -> Option<InputInfo> {
    current_platform().input_init()
}

/// 便捷入口：等价于 `current_platform().sensor_init()`。
pub fn sensor_init() -> Option<SensorInfo> {
    current_platform().sensor_init()
}

/// 便捷入口：等价于 `current_platform().wifi_init()`。
pub fn wifi_init() -> Option<&'static mut dyn WifiDevice> {
    current_platform().wifi_init()
}
