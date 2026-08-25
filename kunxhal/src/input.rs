//! 输入抽象（键盘 / 鼠标 / 触摸）。

/// 输入设备种类。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Keyboard,
    Mouse,
    Touch,
}

/// 输入设备传输方式（决定内核输入驱动如何读取事件）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputTransport {
    /// 主 PL011 UART 上的协议帧（QEMU 简化方案）。
    UartProto,
    /// virtio-input（MMIO/PCI，骨架）。
    Virtio,
    /// I2C 触摸屏（如 Goodix GT911，骨架）。
    I2cGt911,
    /// USB HID 触摸（骨架）。
    UsbHid,
}

/// 输入设备信息（由平台后端返回，供输入驱动初始化）。
pub struct InputInfo {
    /// 设备种类。
    pub kind: InputKind,
    /// 传输方式。
    pub transport: InputTransport,
    /// 设备 MMIO 基地址（UART 协议下复用主 PL011 的基地址）。
    pub base: usize,
    /// 设备中断号（GIC）。
    pub irq: u32,
    /// 触摸面板 X 轴物理最大值（绝对坐标上限，用于物理→屏幕坐标映射）。
    pub max_x: u32,
    /// 触摸面板 Y 轴物理最大值。
    pub max_y: u32,
}

impl InputInfo {
    /// 构造输入设备信息。
    pub const fn new(
        kind: InputKind,
        transport: InputTransport,
        base: usize,
        irq: u32,
        max_x: u32,
        max_y: u32,
    ) -> Self {
        InputInfo { kind, transport, base, irq, max_x, max_y }
    }
}
