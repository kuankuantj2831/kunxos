//! 电源 / 热管理 / DVFS / 电池抽象层。
//!
//! 平台后端（QEMU virt / Raspberry Pi 4）通过实现本模块的 trait 向内核
//! 策略引擎（`kunxcore/src/power.rs`）提供四类能力：
//!
//! 1. [`PowerControl`]：关机 / 重启 / 休眠；
//! 2. [`ThermalSensor`]：SoC 温度读取（毫摄氏度）；
//! 3. [`DvfsController`]：CPU 频点表与频率读写（按簇抽象，预留 big.LITTLE）；
//! 4. [`Battery`]：电池 / 充电状态（无电池平台返回 `None`）。
//!
//! 内核侧的 governor / 热节流 / 电池状态机不依赖任何平台细节，只通过
//! 本模块的 trait 驱动硬件；平台选择与既有 [`crate::Platform`] 一致，
//! 由 Cargo feature 在编译期决定。

/// CPU 频率点（频率 Hz + 建议核心电压 µV）。
///
/// `uv == 0` 表示该频点不管理电压（由固件 / 硬件自动处理）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FreqPoint {
    /// 频率（Hz）。
    pub hz: u32,
    /// 建议核心电压（µV）；0 = 不管理电压。
    pub uv: u32,
}

/// 构造频点（`const` 上下文可用，供静态频点表使用）。
pub const fn freq_point(hz: u32, uv: u32) -> FreqPoint {
    FreqPoint { hz, uv }
}

/// 电源控制接口：平台后端可实现本 trait 提供关机 / 重启 / 休眠能力。
pub trait PowerControl {
    /// 关机（PSCI SYSTEM_OFF）。
    fn shutdown(&self);
    /// 重启（PSCI SYSTEM_RESET）。
    fn reboot(&self);
    /// 休眠（挂起）。当前为占位：无平台后端实现时保持运行。
    fn sleep(&self) {}
}

/// 热传感器接口（SoC 温度等）。
pub trait ThermalSensor {
    /// 传感器数量（BCM2711 的 SoC 温度传感器为 1 个）。
    fn sensor_count(&self) -> usize;
    /// 读取第 `idx` 个传感器的温度，单位毫摄氏度（如 50000 = 50.0 °C）。
    /// 传感器不可用 / 读取失败返回 `None`。
    fn read_temp_mc(&self, idx: usize) -> Option<u32>;
}

/// DVFS 控制器接口。
///
/// 按「簇（cluster）」抽象：同构平台（Pi4 四核 A72）为单簇；big.LITTLE
/// 等异构平台可暴露多个簇，每簇独立频点表与当前频率。策略引擎按簇下发
/// 决策，为后续异构调度预留接口而不影响当前实现。
pub trait DvfsController {
    /// 簇数量（同构平台为 1）。
    fn cluster_count(&self) -> usize;
    /// 第 `cluster` 簇的频点表（按频率升序）。
    fn freq_table(&self, cluster: usize) -> &'static [FreqPoint];
    /// 第 `cluster` 簇当前实际频率（Hz）。
    fn current_hz(&self, cluster: usize) -> u32;
    /// 请求第 `cluster` 簇切换到 `hz`，返回硬件 / 固件实际生效的频率；
    /// 失败（不支持 / 超时）返回 `None`。
    fn set_hz(&self, cluster: usize, hz: u32) -> Option<u32>;
}

/// 电池 / 充电器接口。
///
/// QEMU virt 与 Pi4 均无电池，平台后端返回 `None`；内核侧使用「虚拟电池
/// 模型」进行状态机与事件测试，真机接入时由平台后端实现本 trait。
pub trait Battery {
    /// 电池是否存在。
    fn present(&self) -> bool;
    /// 剩余电量百分比 0..=100。
    fn capacity_percent(&self) -> u8;
    /// 是否正在充电。
    fn charging(&self) -> bool;
    /// 是否连接外部电源（充电器 / USB）。
    fn plugged(&self) -> bool;
    /// 电池电压（mV）。
    fn voltage_mv(&self) -> u32;
}
