//! 电源管理抽象（骨架：当前无平台后端实现电源控制）。

/// 电源操作。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PowerOp {
    Shutdown,
    Reboot,
    Sleep,
}

/// 电源控制接口：平台后端可实现本 trait 以提供关机 / 重启 / 休眠能力。
///
/// 当前为骨架，暂无实现者；方法签名即为后续电源驱动的目标接口。
pub trait PowerControl {
    /// 关机。
    fn shutdown(&self);
    /// 重启。
    fn reboot(&self);
    /// 休眠。
    fn sleep(&self);
}
