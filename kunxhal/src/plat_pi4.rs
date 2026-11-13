//! Raspberry Pi 4（BCM2711）平台后端：硬编码 Pi 4 值。
//!
//! 已实现：
//!   - 显示：通过 VideoCore VI mailbox 属性通道向固件申请 HDMI 帧缓冲，
//!     支持 1080p → 720p → 480p 逐级回退，返回真实物理地址 / 分辨率。
//!   - 网络：GENET 千兆以太网（基址 / 中断 / MAC 信息），驱动接线见内核侧。
//!   - 存储：EMMC2 SD 卡（基址 / 中断 / 块大小），驱动接线见内核侧。
//!   - 输入：Goodix GT911（I2C）或 USB HID 尚未实现，暂返回 `None`。
//!   - 电源：PSCI 0.2（armstub 固件，`smc #0` conduit）关机 / 重启。
//!   - 热：mailbox 属性通道 `GET_TEMP`（tag 0x00030006，id 0 = SoC），
//!     返回毫摄氏度（固件返回 0.001°C 单位，Linux `raspberrypi-hwmon`
//!     与 `vcgencmd measure_temp` 均按毫摄氏度解析）。
//!   - DVFS：mailbox `GET/SET_CLOCK_RATE`（tag 0x00030002 / 0x00038002）
//!     操作 ARM 时钟（clock id 0x3），频点 600/900/1200/1500 MHz；
//!     电压仅实现读取（`GET_VOLTAGE` tag 0x00030003，id 0x1 = Core），
//!     切换电压不在 v1 范围内（避免真机风险）。
//!   - 电池：Pi4 无电池，返回 `None`（内核侧用虚拟电池模型）。
//!
//! 基础设备地址（UART / GIC / RAM / Timer）硬编码自 BCM2711。
//! 中断号取自 Linux `bcm2711.dtsi` 的 `GIC_SPI`（硬件中断号 = 32 + SPI 编号）。

use core::ptr;

use crate::display::DisplayInfo;
use crate::input::InputInfo;
use crate::net::NetInfo;
use crate::plat::Platform;
use crate::power::{freq_point, Battery, DvfsController, FreqPoint, PowerControl, ThermalSensor};
use crate::sensor::{SensorInfo, SensorSource};
use crate::storage::StorageInfo;

/// Raspberry Pi 4 平台单例。
pub static PI4: Pi4Platform = Pi4Platform;

/// Raspberry Pi 4（BCM2711）平台参数。
pub struct Pi4Platform;

// BCM2711 外设 MMIO 基地址。
const MBOX_BASE: usize = 0xFE00_B880; // VideoCore VI mailbox
const GENET_BASE: usize = 0xFD58_0000; // 千兆以太网
const EMMC2_BASE: usize = 0xFE34_0000; // SD 卡（EMMC2）
#[allow(dead_code)]
const XHCI_BASE: usize = 0xFE9C_0000; // USB

impl Platform for Pi4Platform {
    fn name(&self) -> &str {
        "Raspberry Pi 4"
    }

    fn uart_base(&self) -> usize {
        0xFE20_1000 // PL011（亦可用 GPIO 14/15 上的 Mini UART）
    }

    fn uart_irq(&self) -> u32 {
        153 // BCM2711 UART0 (PL011) 中断号（SPI 121 + 32）
    }

    fn uart_clock(&self) -> u32 {
        48_000_000 // BCM2711 clk_uart
    }

    fn gicd_base(&self) -> usize {
        0xFF84_1000 // GIC-400 Distributor（BCM2711 实际基址；0xFF840000 是 GIC 保留区起始，差 0x1000）
    }

    fn gicc_base(&self) -> usize {
        0xFF84_2000 // GIC-400 CPU Interface
    }

    fn ram_base(&self) -> usize {
        0x0000_0000
    }

    fn ram_size(&self) -> usize {
        0x4000_0000 // 1GB（实际 1/2/4/8GB，骨架取 1GB）
    }

    fn timer_freq(&self) -> u64 {
        54_000_000 // BCM2711 系统定时器 54MHz
    }

    fn rtc_base(&self) -> Option<usize> {
        // BCM2711 无 PL031；Pi4 的 RTC 通常通过 I2C 外设（如 DS1307）实现，暂不提供。
        None
    }

    fn display_init(&self) -> Option<DisplayInfo> {
        // 经 VideoCore VI mailbox 属性通道申请 HDMI 帧缓冲（见下方实现）。
        mailbox_framebuffer()
    }

    fn net_init(&self) -> Option<NetInfo> {
        // GENET 千兆以太网（MMIO 0xFD580000）。
        // MAC 实际烧录在 OTP / 由 bootloader 写入 DTB，这里先用 Raspberry Pi
        // 官方 OUI 预留的骨架值，待内核侧 GENET 驱动接线后从寄存器读取。
        Some(NetInfo::new(
            [0xDC, 0xA6, 0x32, 0x00, 0x00, 0x01],
            GENET_BASE,
            189, // GIC_SPI 157 + 32（TX/RX 双中断取主中断）
        ))
    }

    fn storage_init(&self) -> Option<StorageInfo> {
        // EMMC2 SD 卡（MMIO 0xFE340000，标准 SDHCI 寄存器布局）。
        // 内核侧块设备驱动尚未接线，返回信息供后续驱动使用。
        Some(StorageInfo::new(
            EMMC2_BASE,
            158, // GIC_SPI 126 + 32
            512,
        ))
    }

    fn input_init(&self) -> Option<InputInfo> {
        // 骨架：BCM2711 触摸输入经 I2C（Goodix GT911，I2C 地址 0x5D）或 USB HID。
        // 驱动尚未实现，暂返回 None（launcher 检测不到 /dev/input/event0 时
        // 回退为模拟点击）。
        None
    }

    fn sensor_init(&self) -> Option<SensorInfo> {
        // Pi4 无内置传感器：经 BSC1（I2C1，GPIO 2/3）外接 MPU6050/MPU9250。
        // 真实器件探测（WHO_AM_I）在驱动层完成；探测失败回退到软件模拟器。
        Some(SensorInfo::new(
            SensorSource::Mpu9250,      // 默认按 9250（含磁力计）；6050 也兼容
            crate::sensor::BSC1_BASE,   // BSC1（I2C1）
            crate::sensor::BSC1_IRQ,    // GIC SPI 109 + 32（当前轮询，预留中断）
            100_000_000,                // BCM2711 BSC 核心时钟（Linux 树默认 100MHz）
            0x68,                       // MPU 默认地址（AD0 接地）
            0x0C,                       // AK8963 磁力计地址
        ))
    }

    fn power_control(&self) -> Option<&'static dyn PowerControl> {
        Some(&PI4_POWER)
    }

    fn thermal_sensor(&self) -> Option<&'static dyn ThermalSensor> {
        Some(&PI4_THERMAL)
    }

    fn dvfs_controller(&self) -> Option<&'static dyn DvfsController> {
        Some(&PI4_DVFS)
    }

    fn battery(&self) -> Option<&'static dyn Battery> {
        None // Pi4 无电池
    }

    fn core_voltage_mv(&self) -> Option<u32> {
        pi4_core_voltage_mv()
    }

    fn wifi_init(&self) -> Option<&'static mut dyn crate::wifi::WifiDevice> {
        // CYW43455 SDIO 后端骨架（BCM2711 mmc@0xFE300000；硬件接线待真机验证，
        // 见文件尾部「Wi-Fi：CYW43455 SDIO 后端」一节）。
        Some(cyw_wifi())
    }
}

// ==================== VideoCore VI mailbox 属性通道 ====================
//
// Mailbox 位于 0xFE00B880：ARM 侧通过"写 → 读"一对寄存器与 VideoCore 交换
// 消息。属性通道（channel 8）用于向固件查询/请求设备参数（帧缓冲等）。
// 消息缓冲须 16 字节对齐，且地址须为 VC 视角的 bus 地址（0xC0000000 | 物理地址）。

const MBOX_CHANNEL_PROPERTY: u32 = 8;
const MBOX_STATUS_FULL: u32 = 1 << 31; // 写满
const MBOX_STATUS_EMPTY: u32 = 1 << 30; // 读空
const MBOX_BUS_BASE: u32 = 0xC000_0000; // ARM 物理地址 → VC bus 地址偏移

/// 属性消息缓冲（16 字节对齐，256 字节，足够装下帧缓冲申请序列）。
#[repr(align(16))]
struct MboxBuf([u32; 64]);

/// 向 mailbox 写入一个值（等待不忙）。
///
/// BCM2711 的 mailbox 寄存器布局与 BCM2835 不同：STATUS=0x18、WRITE=0x20
/// （Linux bcm2835-mailbox.c 使用 0x18/0x20，可在 Pi 4 真机运行）。
fn mbox_write(value: u32) -> bool {
    let status = (MBOX_BASE + 0x18) as *const u32;
    let write = (MBOX_BASE + 0x20) as *mut u32;
    for _ in 0..1_000_000 {
        if unsafe { ptr::read_volatile(status) } & MBOX_STATUS_FULL == 0 {
            unsafe { ptr::write_volatile(write, value) };
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// 从 mailbox 读取一个值（等待不空，仅接收属性通道消息）。
fn mbox_read() -> Option<u32> {
    let status = (MBOX_BASE + 0x18) as *const u32;
    let read = (MBOX_BASE + 0x00) as *const u32;
    for _ in 0..1_000_000 {
        if unsafe { ptr::read_volatile(status) } & MBOX_STATUS_EMPTY == 0 {
            let data = unsafe { ptr::read_volatile(read) };
            if data & 0xF == MBOX_CHANNEL_PROPERTY {
                return Some(data & !0xF);
            }
            // 其它通道的消息：继续等待
        }
        core::hint::spin_loop();
    }
    None
}

/// 按 cache line（64B）对一段内存执行 DC CVAC（clean 到 PoC，供 VC 读取）。
fn clean_dcache(addr: usize, len: usize) {
    let line = 64usize;
    let start = addr & !(line - 1);
    let end = (addr + len + line - 1) & !(line - 1);
    let mut a = start;
    while a < end {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("dc cvac, {0}", in(reg) a, options(nostack, preserves_flags));
        }
        a += line;
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// 按 cache line（64B）对一段内存执行 DC IVAC（invalidate，读回 VC 写入）。
fn invalidate_dcache(addr: usize, len: usize) {
    let line = 64usize;
    let start = addr & !(line - 1);
    let end = (addr + len + line - 1) & !(line - 1);
    let mut a = start;
    while a < end {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("dc ivac, {0}", in(reg) a, options(nostack, preserves_flags));
        }
        a += line;
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// 属性通道调用：`buf[0]` = 消息字节数、`buf[1]` = 0（请求）。
/// 调用前 clean 缓冲（VC 读取），成功后 invalidate 并检查响应码，返回是否成功。
fn property_call(buf: &mut MboxBuf) -> bool {
    let phys = buf as *mut MboxBuf as u32;
    let bus = phys | MBOX_BUS_BASE;
    clean_dcache(phys as usize, 256); // clean 到 PoC，VC 才能看到请求
    if !mbox_write(bus) {
        return false;
    }
    loop {
        match mbox_read() {
            Some(resp) if resp == bus => break,
            Some(_) => continue, // 其它通道响应，丢弃
            None => return false,
        }
    }
    invalidate_dcache(phys as usize, 256); // invalidate，读回 VC 写入的响应
    unsafe { buf.0[1] & 0x8000_0000 != 0 }
}

/// 向属性消息追加一个 tag：`(tag_id, value_size, value_len, values...)`。
fn push_tag(buf: &mut [u32], pos: &mut usize, tag: u32, values: &[u32]) {
    let n = values.len();
    buf[*pos] = tag;
    *pos += 1;
    buf[*pos] = (n * 4) as u32; // value 缓冲大小（字节）
    *pos += 1;
    buf[*pos] = (n * 4) as u32; // value 长度（请求时 = 缓冲区大小）
    *pos += 1;
    for v in values {
        buf[*pos] = *v;
        *pos += 1;
    }
}

/// 申请 HDMI 帧缓冲：尝试 1080p → 720p → 480p，返回首个成功的结果。
fn mailbox_framebuffer() -> Option<DisplayInfo> {
    const MODES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (640, 480)];
    for (w, h) in MODES {
        if let Some(info) = try_framebuffer(w, h) {
            return Some(info);
        }
    }
    None
}

/// 以指定分辨率向固件申请帧缓冲，成功返回 [`DisplayInfo`]。
fn try_framebuffer(w: u32, h: u32) -> Option<DisplayInfo> {
    let mut buf = MboxBuf([0; 64]);
    let mut pos = 2usize; // buf[0] 消息长度、buf[1] 请求码 0，先留空
    push_tag(&mut buf.0, &mut pos, 0x0004_8003, &[w, h]); // 设置物理分辨率
    push_tag(&mut buf.0, &mut pos, 0x0004_8004, &[w, h]); // 设置虚拟分辨率
    push_tag(&mut buf.0, &mut pos, 0x0004_8005, &[32]); // 设置深度 32bpp
    push_tag(&mut buf.0, &mut pos, 0x0004_8009, &[0, 0]); // 虚拟偏移 (0,0)
    push_tag(&mut buf.0, &mut pos, 0x0004_0001, &[4096, 0]); // 分配帧缓冲：对齐 4096，大小 0 由固件决定
    push_tag(&mut buf.0, &mut pos, 0x0004_0008, &[0]); // 获取行字节数（pitch）
    buf.0[pos] = 0; // 结束 tag
    pos += 1;
    buf.0[0] = ((pos * 4 + 15) & !15) as u32; // 消息总字节数（16 对齐）

    if !property_call(&mut buf) {
        return None;
    }

    // 解析响应：遍历 tag，取分配到的 bus 地址 / 大小与 pitch。
    let mut p = 2usize;
    let mut fb_bus = 0u32;
    let mut pitch = 0u32;
    // 边界防护：畸形固件响应（val_size 巨大/越界 p）此前直接索引越界 panic，
    // 且发生在 display_init 启动路径。
    while p + 2 < buf.0.len() {
        let tag = buf.0[p];
        p += 1;
        if tag == 0 {
            break;
        }
        let val_size = buf.0[p];
        p += 1;
        if p >= buf.0.len() || val_size as usize > buf.0.len() * 4 {
            break;
        }
        let val_len = buf.0[p];
        p += 1;
        if p + 1 >= buf.0.len() {
            break;
        }
        if tag == 0x0004_0001 && val_len >= 12 {
            fb_bus = buf.0[p + 1]; // [对齐, bus 地址, 大小]
        } else if tag == 0x0004_0008 && val_len >= 4 {
            pitch = buf.0[p]; // [行字节数]
        }
        p += (val_size / 4) as usize;
    }

    if fb_bus == 0 {
        return None; // 固件未分配（分辨率不被支持等）
    }
    // VC 返回 bus 地址，低 1GB 掩码还原物理地址（1GB 变体）。
    let fb_phys = (fb_bus & 0x3FFF_FFFF) as usize;
    let mut info = DisplayInfo::new(fb_phys, w, h, 32);
    info.stride_bytes = pitch.max(w * 4);
    Some(info)
}

// ==================== 电源 / 热 / DVFS（mailbox + PSCI） ====================

/// 通用单 tag 属性查询：构造 `[tag, req...]` 消息经属性通道发给固件，
/// 返回响应中的值数组（去掉 tag 头）。失败 / 响应码错误返回 `None`。
fn property_query(tag: u32, req: &[u32]) -> Option<[u32; 4]> {
    let mut buf = MboxBuf([0; 64]);
    let mut pos = 2usize; // buf[0] 消息长度、buf[1] 请求码 0
    push_tag(&mut buf.0, &mut pos, tag, req);
    buf.0[pos] = 0; // 结束 tag
    pos += 1;
    buf.0[0] = ((pos * 4 + 15) & !15) as u32;
    if !property_call(&mut buf) {
        return None;
    }
    // 解析响应：`[tag, value_buffer_size, value_length, values...]`，
    // value_length 高位置 1 表示成功，低 31 位为响应数据字节数。
    let mut p = 2usize;
    if buf.0[p] != tag {
        return None;
    }
    p += 1;
    let val_size = buf.0[p];
    p += 1;
    let val_len = buf.0[p];
    p += 1;
    if val_len & 0x8000_0000 == 0 {
        return None; // 固件返回失败
    }
    let n = ((val_len & 0x7FFF_FFFF) / 4).min(4) as usize;
    let mut out = [0u32; 4];
    for i in 0..n {
        out[i] = buf.0[p + i];
    }
    let _ = val_size;
    Some(out)
}

/// PSCI 0.2 函数 ID（armstub 固件，`smc #0` conduit；与 smp.rs 的
/// `psci_call` 一致 —— 真机 Pi4 由 VideoCore 固件提供 PSCI）。
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

/// Pi4 PSCI 电源控制单例。
pub static PI4_POWER: Pi4Power = Pi4Power;

/// Pi4 电源控制：`smc #0` 交给 EL3 armstub（PSCI）。
pub struct Pi4Power;

impl PowerControl for Pi4Power {
    fn shutdown(&self) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("smc #0", in("x0") PSCI_SYSTEM_OFF, options(noreturn));
        }
        #[cfg(not(target_arch = "aarch64"))]
        loop {}
    }

    fn reboot(&self) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("smc #0", in("x0") PSCI_SYSTEM_RESET, options(noreturn));
        }
        #[cfg(not(target_arch = "aarch64"))]
        loop {}
    }

    // sleep：Pi4 无固件级挂起支持（需 mailbox 或寄存器控制，v1 占位）。
}

/// 温度传感器 id（0 = SoC，1 = 新固件上的 Display 等，取 0）。
const TEMP_ID_SOC: u32 = 0;

/// Pi4 热传感器单例。
pub static PI4_THERMAL: Pi4Thermal = Pi4Thermal;

/// Pi4 热传感器：mailbox `GET_TEMP`（0x00030006）。
pub struct Pi4Thermal;

impl ThermalSensor for Pi4Thermal {
    fn sensor_count(&self) -> usize {
        1
    }

    fn read_temp_mc(&self, idx: usize) -> Option<u32> {
        if idx != 0 {
            return None;
        }
        // 请求 `[temperature_id]`，响应 `[temperature_id, temp_mc]`。
        let r = property_query(0x0003_0006, &[TEMP_ID_SOC])?;
        Some(r[1])
    }
}

/// ARM 时钟 id（mailbox Clock IDs 表：0x3 = ARM）。
const CLOCK_ID_ARM: u32 = 0x3;
/// Core 电压 id（Voltage IDs 表：0x1 = Core）。
const VOLTAGE_ID_CORE: u32 = 0x1;
/// GET_CLOCK_RATE（0x00030002）：[clock_id] → [clock_id, rate_hz]。
const TAG_GET_CLOCK_RATE: u32 = 0x0003_0002;
/// SET_CLOCK_RATE（0x00038002）：[clock_id, rate_hz] → [clock_id, rate_hz]。
const TAG_SET_CLOCK_RATE: u32 = 0x0003_8002;
/// GET_VOLTAGE（0x00030003）：[voltage_id] → [voltage_id, value]，
/// value = 相对 1.2V 的 0.025V 步进偏移。
const TAG_GET_VOLTAGE: u32 = 0x0003_0003;

/// Pi4 Cortex-A72 频点表（600/900/1200/1500 MHz，与固件支持的 100MHz
/// 步进一致）。`uv` 填 0：v1 只切频不切压（核心电压由固件自动管理）。
static PI4_FREQ_TABLE: [FreqPoint; 4] = [
    freq_point(600_000_000, 0),
    freq_point(900_000_000, 0),
    freq_point(1_200_000_000, 0),
    freq_point(1_500_000_000, 0),
];

/// Pi4 DVFS 控制器单例。
pub static PI4_DVFS: Pi4Dvfs = Pi4Dvfs;

/// Pi4 DVFS 控制器：mailbox `GET/SET_CLOCK_RATE` 操作 ARM 时钟。
///
/// 固件会把请求频率 clamp 到实际支持的范围内并返回生效值；切换是
/// 同步的（VideoCore 收到请求后完成 PLL 配置）。
pub struct Pi4Dvfs;

impl DvfsController for Pi4Dvfs {
    fn cluster_count(&self) -> usize {
        1 // 四核 A72 同构，单簇
    }

    fn freq_table(&self, cluster: usize) -> &'static [FreqPoint] {
        if cluster == 0 {
            &PI4_FREQ_TABLE
        } else {
            &[]
        }
    }

    fn current_hz(&self, cluster: usize) -> u32 {
        if cluster != 0 {
            return 0;
        }
        match property_query(TAG_GET_CLOCK_RATE, &[CLOCK_ID_ARM]) {
            Some(r) => r[1],
            None => 0,
        }
    }

    fn set_hz(&self, cluster: usize, hz: u32) -> Option<u32> {
        if cluster != 0 {
            return None;
        }
        let r = property_query(TAG_SET_CLOCK_RATE, &[CLOCK_ID_ARM, hz])?;
        Some(r[1])
    }
}

/// 读取 Core 电压（mV）。仅用于状态显示；v1 不执行电压切换。
pub fn pi4_core_voltage_mv() -> Option<u32> {
    // value = 相对 1.2V 的 0.025V 步进**有符号**偏移（mailbox 返回 i32）：
    // mV = 1200 + value * 25。按 u32 直接 saturating_mul 在负偏移（核心电压
    // <1.2V，如 0xFFFFFFFC）下会饱和成 u32::MAX 再回绕出错误读数。
    let r = property_query(TAG_GET_VOLTAGE, &[VOLTAGE_ID_CORE])?;
    if r[1] == 0x8000_0000 {
        return None; // id 无效
    }
    let off = r[1] as i32;
    let mv = (1200i32).saturating_add(off.saturating_mul(25));
    if mv < 0 {
        return None;
    }
    Some(mv as u32)
}

// ==================== 相机：Pi4 硬件后端（PiCamera 骨架） ====================
//
// Raspberry Pi 4（BCM2711）相机硬件路径（真机验证待完成，见 docs/camera.md
// 测试方案与「关键问题」解答）：
//
//   1. 传感器（IMX219 / IMX708）经 I2C0（BSC0 @ 0xFE205000，GPIO 44/45）
//      配置：写寄存器（分辨率 / 曝光 / 增益 / 启动 streaming）
//   2. MIPI CSI-2 发送：传感器把 Bayer RAW（RAW10）经 2-lane CSI 发往 SoC
//   3. 接收：BCM2711 CSI-2 接收器（Unicam）@ 0xFE801000（CSI0 / CAM1 连接器）
//      把 D-PHY 数据收进内存（DMA 到帧缓冲），ISP 未实现 → RAW/YUV 直出
//
// 当前状态：本骨架完成设备抽象接线（open/configure/3A 状态机真实生效），
// 采集路径（start_stream / capture_single / read_frame）在**固件 MMAL/IL 或
// Unicam 寄存器驱动接线完成前**返回 [`CameraError::NotImplemented`]。
// 寄存器常量与初始化序列按 BCM2711 外设手册（§7.2 CSI-2 / §3.2 BSC）与
// IMX219 数据手册整理，供后续驱动接线使用。

use crate::camera::{
    CameraConfig, CameraDevice, CameraDeviceInfo, CameraError, CameraFacing, CaptureRequest,
    CaptureResult, FixedThreeA, PixelFormat, ThreeA, ThreeAParams,
};

/// Pi4 相机最大分辨率（IMX219 全分辨率）。
pub const PI_CAM_MAX_W: usize = 3280;
pub const PI_CAM_MAX_H: usize = 2464;

/// Pi4 相机单例。
pub static mut PI_CAMERA: PiCamera = PiCamera::new();

/// Pi4 相机后端：设备状态真实管理，采集路径待硬件接线。
pub struct PiCamera {
    open: bool,
    streaming: bool,
    config: CameraConfig,
    threea: FixedThreeA,
}

impl PiCamera {
    pub const fn new() -> Self {
        PiCamera {
            open: false,
            streaming: false,
            config: CameraConfig::new(640, 480, 30, PixelFormat::RawBayer8),
            threea: FixedThreeA::new(ThreeAParams::defaults()),
        }
    }
}

impl CameraDevice for PiCamera {
    fn enumerate(&self, out: &mut [CameraDeviceInfo]) -> usize {
        if out.is_empty() {
            return 0;
        }
        // CAM1 连接器（CSI0 + I2C0）：默认接 Camera Module v2（IMX219）。
        out[0] = CameraDeviceInfo {
            id: 0,
            name: "csi0-imx219",
            facing: CameraFacing::Back,
            max_width: PI_CAM_MAX_W,
            max_height: PI_CAM_MAX_H,
            formats: 0b011, // raw8 | yuv420（RGB888 由软件转换）
        };
        1
    }

    fn open(&mut self, id: u32) -> Result<(), CameraError> {
        if id != 0 {
            return Err(CameraError::NotFound);
        }
        if self.open {
            return Err(CameraError::Busy);
        }
        self.open = true;
        Ok(())
    }

    fn close(&mut self) -> Result<(), CameraError> {
        self.streaming = false;
        self.open = false;
        Ok(())
    }

    fn configure(&mut self, config: &CameraConfig) -> Result<(), CameraError> {
        if !self.open {
            return Err(CameraError::InvalidArg);
        }
        if config.width == 0 || config.height == 0 || config.fps == 0 {
            return Err(CameraError::InvalidArg);
        }
        if config.width > PI_CAM_MAX_W || config.height > PI_CAM_MAX_H {
            return Err(CameraError::Unsupported);
        }
        self.config = *config;
        Ok(())
    }

    fn start_stream(&mut self) -> Result<(), CameraError> {
        if !self.open {
            return Err(CameraError::InvalidArg);
        }
        // TODO(硬件接线)：配置 IMX219 分辨率/曝光并启动 streaming，
        // 编程 Unicam（CSI_CTRL 使能、2-lane、RAW10 数据类型、DMA 到帧缓冲），
        // 然后置 streaming = true。
        Err(CameraError::NotImplemented)
    }

    fn stop_stream(&mut self) -> Result<(), CameraError> {
        self.streaming = false;
        Ok(())
    }

    fn poll_frame(&mut self) -> Option<CaptureResult> {
        // TODO(硬件接线)：Unicam DMA 完成中断 / 轮询 CSI DMA 状态后填帧。
        None
    }

    fn capture_single(&mut self, _req: &CaptureRequest) -> Result<CaptureResult, CameraError> {
        Err(CameraError::NotImplemented)
    }

    fn get_3a(&self) -> ThreeAParams {
        self.threea.params()
    }

    fn set_3a(&mut self, params: ThreeAParams) {
        self.threea.set(params);
    }

    fn read_frame(
        &mut self,
        _result: &CaptureResult,
        _dst: &mut [u8],
    ) -> Result<usize, CameraError> {
        Err(CameraError::NotImplemented)
    }
}

// ==================== 硬件接线参考（常量与初始化序列，驱动实现时使用） ====================

/// BSC0（I2C0）控制器：CAM1 连接器上相机传感器的 I2C 总线（GPIO 44/45）。
pub const PI_I2C0_BASE: usize = 0xFE20_5000;

/// MIPI CSI-2 接收器（Unicam）基址：CSI0 接 CAM1 连接器（2-lane）。
pub const PI_CSI0_BASE: usize = 0xFE80_1000;
/// CSI1 接 CAM0 连接器（2-lane，双摄预留）。
pub const PI_CSI1_BASE: usize = 0xFE80_2000;

/// Unicam 关键寄存器偏移（BCM2711 外设手册 §7.2 CSI-2；寄存器名与 Linux
/// `bcm2835-unicam` 驱动一致，驱动接线时以 datasheet 为准核对）。
#[allow(dead_code)]
pub mod unicam_regs {
    /// CSI_CTRL0：全局控制（使能 / 软复位）。
    pub const CTRL0: usize = 0x00;
    /// CSI_CTRL1：PHY 控制（使能 D-PHY、lane 数、连续时钟）。
    pub const CTRL1: usize = 0x04;
    /// CSI_CTRL2：协议控制（数据类型 RAW10/RAW8、帧起始/结束掩码）。
    pub const CTRL2: usize = 0x08;
    /// CSI_CTRL3：打包控制。
    pub const CTRL3: usize = 0x0C;
    /// CSI_IMAGE_SIZE：图像宽高。
    pub const IMAGE_SIZE: usize = 0x10;
    /// CSI_DATA_CFG：行/帧配置。
    pub const DATA_CFG: usize = 0x1C;
    /// CSI_DMA_ADDR：DMA 目标地址（帧缓冲物理地址）。
    pub const DMA_ADDR: usize = 0x20;
    /// CSI_DMA_CTRL：DMA 使能 / 状态。
    pub const DMA_CTRL: usize = 0x24;
    /// CSI_DMA_STATUS：DMA 完成 / 错误状态。
    pub const DMA_STATUS: usize = 0x28;
}

/// IMX219（Camera Module v2）关键寄存器（16bit 寄存器地址，16bit 值）。
#[allow(dead_code)]
pub mod imx219_regs {
    /// 芯片 ID 寄存器（只读）：高字节 0x02、低字节 0x19。
    pub const CHIP_ID_HI: u16 = 0x00;
    pub const CHIP_ID_LO: u16 = 0x01;
    /// 模式选择：0x01 启动 streaming，0x00 停止。
    pub const MODE_SELECT: u16 = 0x01;
    /// 行长度（单位：PLL 时钟周期）。
    pub const LINE_LENGTH: u16 = 0x01_62;
    /// 帧长度（帧周期）。
    pub const FRAME_LENGTH: u16 = 0x01_63;
    /// 曝光时间（行单位）。
    pub const EXPOSURE: u16 = 0x01_5A;
    /// 增益（7.3 定点）。
    pub const GAIN: u16 = 0x01_5E;
    /// 数字增益 R/G/B。
    pub const DIG_GAIN_R: u16 = 0x01_5C;
    pub const DIG_GAIN_G: u16 = 0x01_5D;
    pub const DIG_GAIN_B: u16 = 0x01_5F;
}

/// 固件探测：经 VideoCore mailbox 读取内核命令行，检测相机是否使能。
///
/// Linux 在 `/boot/config.txt` 用 `camera_auto_detect=1` 使能相机；裸机
/// KunxOS 下由 bootloader 传入的 DTB / 固件配置决定。此处演示探测逻辑
/// （读取 `TAG_GET_COMMAND_LINE` 0x00050001），未检测到时返回 `false`。
#[allow(dead_code)]
pub fn pi4_firmware_camera_enabled() -> bool {
    // TODO(硬件接线)：属性通道读取命令行（256B 缓冲，需扩展 MboxBuf），
    // 扫描 "camera_auto_detect=1" 或 DTB /chosen 中的相机节点。
    false
}

/// 初始化序列大纲（驱动接线时按序执行）：
/// 1. GPIO 44/45 复用为 ALT0（I2C0），配置 BSC0 时钟 100kHz
/// 2. I2C 读 IMX219 CHIP_ID（0x0000 → 0x0219），不符则报 NotFound
/// 3. 写 LINE_LENGTH / FRAME_LENGTH / EXPOSURE / GAIN / DIG_GAIN（3A 默认值）
/// 4. 写 MODE_SELECT = 0x01 启动 streaming
/// 5. 编程 Unicam：CTRL1 使能 D-PHY + 2 lane，CTRL2 设 RAW10 数据类型，
///    IMAGE_SIZE 写宽高，DMA_ADDR 写帧缓冲物理地址，DMA_CTRL 使能
/// 6. 轮询 DMA_STATUS 出帧（或接 GIC 中断），帧数据 = RAW10 Bayer
///
/// RAW10 → RAW8 / YUV 转换与 ISP（去马赛克、AWB 增益）属后续工作；
/// 3A 先用 [`FixedThreeA`] 固定参数（见 docs/camera.md）。
#[allow(dead_code)]
pub const PI4_CAMERA_INIT_NOTES: &str = "see docs/camera.md §Pi4 硬件接线";

// ==================== Wi-Fi：CYW43455 SDIO 后端（CywWifi 骨架） ====================
//
// Raspberry Pi 4（BCM2711）无线网卡是 Cypress（Broadcom）CYW43455
// （WiFi/蓝牙组合芯片），挂在 BCM2711 的 SDIO 控制器上：
//
//   BCM2711 mmc@7e300000（DesignWare MMC，"brcm,bcm2835-mmc"）@ 0xFE300000
//   中断 GIC_SPI(41) → 硬件号 73；与 SD 卡（EMMC2 @ 0xFE340000）分属两条总线。
//   CYW43455 SDIO 功能分配：功能 1 = 回板（backplane）总线，功能 2 = 数据帧，
//   功能 3 = 调试/事件（实际事件在功能 2 数据头中携带）。
//
// 架构（Linux brcmfmac 同款划分，自研极简版）：
//   1. SDIO 基础层：dwmmc 控制器初始化 + CMD52/CMD53（IO_RW_DIRECT/EXTENDED）
//      + 回板窗口访问（SBSDIO_FUNC1_SBADDR 指向 0x18000000|addr）。
//   2. 固件下载：芯片复位 → dongle 启动（F1 可用）→ dongle RAM 上传固件
//      （brcmfmac43455-sdio.bin，闭源 blob，~500KB）→ CLM blob → NVRAM 配置。
//   3. 命令/事件接口：SDPCM 帧头（B 消息 = 总线控制，I 消息 = ioctl，
//      A 消息 = 事件）+ WLC ioctl（SCAN / JOIN / DISASSOC / UP ...）
//      + WLC 事件（SCAN_COMPLETE / ASSOC / PSK_SUP ...）轮询环。
//   4. 数据通路：802.3 以太网帧（EAPOL 0x888E 中继给内核侧 4-way）。
//
// 当前状态（与相机 Pi4 骨架一致的诚实标注）：
//   - SDIO 基础层（dwmmc 寄存器 + CMD52/53 参数编码）已实现，供真机接线；
//   - 固件下载与 WLC ioctl/事件环为本骨架的「下一步接线」部分：相关常量与
//     时序按 brcmfmac / wlioctl 提供，函数返回 [`WifiError::NotImplemented`]
//     或 `NotAvailable`，直到真机逐项验证。
//   - 固件 blob 由内核从 /boot（FAT32，挂载于 /mnt/fat32）读取后经
//     [`cyw_wifi_set_firmware`] 注入（blob 闭源，无法内嵌进仓库）。

use crate::wifi::{WifiDevice, WifiError, WifiLinkState, WifiScanPoll, WifiScanResult, WifiStatus};

/// BCM2711 SDIO 控制器（无线网卡所在）基址。
pub const CYW_SDIO_BASE: usize = 0xFE30_0000;
/// BCM2711 SDIO 控制器中断号（GIC_SPI 41 + 32）。
pub const CYW_SDIO_IRQ: u32 = 73;

// ==================== dwmmc（DesignWare MMC）寄存器 ====================
// 偏移以 Linux drivers/mmc/host/dw_mmc.h 为准；真机接线时对照 BCM2711 手册核对。

#[allow(dead_code)]
pub mod dwmmc_regs {
    pub const CTRL: usize = 0x00;
    pub const PWEN: usize = 0x04;
    pub const CLKDIV: usize = 0x08;
    pub const CLKSRC: usize = 0x0C;
    pub const CMD: usize = 0x10;
    pub const ARG: usize = 0x14;
    pub const STATUS: usize = 0x20;
    pub const IRPTEN: usize = 0x24;
    pub const IRPTMASK: usize = 0x28;
    pub const FIFOCNT: usize = 0x2C;
    pub const FIFO: usize = 0x30;
    pub const RESP0: usize = 0x34;
    pub const RESP1: usize = 0x38;
    pub const RESP2: usize = 0x3C;
    pub const RESP3: usize = 0x40;
    pub const BLKSIZ: usize = 0xE8;
    pub const BYTCNT: usize = 0xEC;
    pub const CTLR1: usize = 0xF0;
    pub const DMACTRL: usize = 0xF8;
    pub const DMAADDR: usize = 0xFC;
}

/// dwmmc CMD 寄存器位。
#[allow(dead_code)]
pub mod dwmmc_cmd {
    pub const START: u32 = 0x8000_0000;
    pub const RESP_EXP: u32 = 0x0000_0040; // 期望响应
    pub const RESP_LONG: u32 = 0x0000_0080; // 136 位长响应
    pub const DATA_EXP: u32 = 0x0020_0000; // 期望数据传输
    pub const WAIT_PRVDATA: u32 = 0x0000_0020; // 等待前一次数据完成
    pub const INDEX_SHIFT: u32 = 24;
}

/// dwmmc STATUS 位（基本位；FIFO 计数在高半区，见 FIFOCNT 更直接）。
#[allow(dead_code)]
pub mod dwmmc_sta {
    pub const CMD_ACTIVE: u32 = 0x0000_0001;
    pub const DAT_ACTIVE: u32 = 0x0000_0002;
    pub const FIFO_FULL: u32 = 0x0000_0008;
    pub const FIFO_EMPTY: u32 = 0x0000_0010;
}

// ==================== SDIO 命令参数编码（SDIO 简版规范 v3.0） ====================
// 位布局：CMD52/CMD53 参数按表 4-4/4-6 定义，寄存器地址为 17 位（低位）。

/// 构造 CMD52（IO_RW_DIRECT）参数。
/// 返回值 0xFF 与 `write=false` 用于读；`write=true` 时低位 8 位为写数据。
fn sdio_cmd52_arg(func: u8, addr: u32, write: bool, data: u8) -> u32 {
    let mut arg = (data as u32) & 0xFF; // [7:0] 写数据
    arg |= (addr & 0x1FF) << 9; // [16:9] 寄存器地址（17 位中的低 8 位窗口）
    arg |= ((func & 0x7) as u32) << 14; // [16:14]？见下方注释
    arg |= if write { 1 << 17 } else { 0 }; // [17] RAW 位（写请求）
    arg
}

// 注释（真机核对点）：SDIO 简版规范 CMD52 参数位为
//   [7:0] 写数据 | [9:8] 保留 | [12:10] 保留 | [16:13] 功能号 | [17] RAW(写)
//   | [25:18] 保留 | [31:26] 保留；寄存器地址来自 CMD52 前面的 I/O 地址
//   寄存器设置（FBR），此处保留简洁封装，接线时按规范逐位核对。

/// 构造 CMD53（IO_RW_EXTENDED）参数。
/// `read=true` 表示 CMD53 读（OP Code 1），`block=true` 表示块模式；count 为
/// 字节/块数。
fn sdio_cmd53_arg(func: u8, addr: u32, read: bool, block: bool, count: u16) -> u32 {
    let mut arg = (count & 0x1FF) as u32; // [8:0] 字节/块数
    arg |= (addr & 0x1FFFF) << 9; // [25:9] 17 位寄存器地址
    arg |= ((func & 0x7) as u32) << 26; // [28:26] 功能号
    arg |= if read { 1 << 29 } else { 0 }; // [29] OP Code（1=读）
    arg |= (block as u32) << 30; // [30] 块模式
    arg
}

// ==================== dwmmc 基础操作 ====================

/// 读 dwmmc 32 位寄存器。
fn dw_reg(off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((CYW_SDIO_BASE + off) as *const u32) }
}

/// 写 dwmmc 32 位寄存器。
fn dw_wreg(off: usize, v: u32) {
    unsafe { core::ptr::write_volatile((CYW_SDIO_BASE + off) as *mut u32, v) }
}

/// dwmmc 控制器探测：读版本/控制寄存器是否可访问（真机上 0xFE300000 由
/// bootloader 完成电力与时钟后即可读到非 0；QEMU 下该地址未映射）。
fn dw_present() -> bool {
    // CTRL 至少低 2 位可控（控制器使能/复位置位后会有反应）；此处仅做
    // 「能读到值」的探测。
    dw_reg(dwmmc_regs::CTRL) != 0xFFFF_FFFF
}

/// 初始化 dwmmc 控制器（时钟分频 + 使能）。
fn dw_init() -> bool {
    if !dw_present() {
        return false;
    }
    // 时钟源选择与分频：BCM2711 上 dwmmc 走 APB/固定 200MHz 派生；CLKDIV
    // 目标 25MHz 起始。真机接线时按手册核对 CLKSRC 与分频公式。
    dw_wreg(dwmmc_regs::CLKDIV, 8);
    dw_wreg(dwmmc_regs::PWEN, 1);
    dw_wreg(dwmmc_regs::CTRL, 1); // 控制器使能
    true
}

/// 发送一条 SD/MMC 命令（低级，供 CMD52/CMD53 组装使用）。
/// `expect_resp=true` 时等待响应完成并返回 RESP0；`expect_data` 仅标志位。
#[allow(dead_code)]
fn dw_send_cmd(index: u8, arg: u32, expect_resp: bool, expect_data: bool) -> Option<u32> {
    use dwmmc_cmd::*;
    if !dw_present() {
        return None;
    }
    dw_wreg(dwmmc_regs::ARG, arg);
    let mut cmd = START | ((index as u32) << INDEX_SHIFT);
    if expect_resp {
        cmd |= RESP_EXP;
    }
    if expect_data {
        cmd |= DATA_EXP | WAIT_PRVDATA;
    }
    dw_wreg(dwmmc_regs::CMD, cmd);
    // 等待命令不再活动（简单轮询；真机接线按 IRPTMASK + 中断完成）
    for _ in 0..100_000 {
        if dw_reg(dwmmc_regs::STATUS) & dwmmc_sta::CMD_ACTIVE == 0 {
            if expect_resp {
                return Some(dw_reg(dwmmc_regs::RESP0));
            }
            return Some(0);
        }
        core::hint::spin_loop();
    }
    None
}

/// 简单 FIFO PIO 写入（主机 → 卡片，CMD53 写数据）。
/// 真机接线：按 FIFOCNT/STATUS 等待可写后逐 32 位字写入 FIFO（见 docs/wifi.md）。
#[allow(dead_code)]
fn dw_fifo_write(buf: &[u8]) -> bool {
    if !dw_present() {
        return false;
    }
    let words = (buf.len() + 3) / 4;
    let mut pos = 0usize;
    for _ in 0..words {
        for _ in 0..10_000 {
            let s = dw_reg(dwmmc_regs::STATUS);
            if s & dwmmc_sta::FIFO_FULL == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        let mut word = [0u8; 4];
        let n = buf.len().saturating_sub(pos).min(4);
        word[..n].copy_from_slice(&buf[pos..pos + n]);
        dw_wreg(dwmmc_regs::FIFO, u32::from_le_bytes(word));
        pos += 4;
    }
    true
}

/// 简单 FIFO PIO 读取（卡片 → 主机，CMD53 读数据）。
#[allow(dead_code)]
fn dw_fifo_read(dst: &mut [u8]) -> bool {
    if !dw_present() {
        return false;
    }
    let words = (dst.len() + 3) / 4;
    let mut pos = 0usize;
    for _ in 0..words {
        for _ in 0..10_000 {
            let s = dw_reg(dwmmc_regs::STATUS);
            if s & dwmmc_sta::FIFO_EMPTY == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        let w = dw_reg(dwmmc_regs::FIFO).to_le_bytes();
        let n = dst.len().saturating_sub(pos).min(4);
        dst[pos..pos + n].copy_from_slice(&w[..n]);
        pos += 4;
    }
    true
}

// ==================== CYW43455 常量（cccr / 回板 / SDPCM / WLC） ====================

/// CCCR（Card Common Control Register）地址（SDIO 简版规范 §6.1）。
#[allow(dead_code)]
pub mod cccr {
    pub const IO_0: u32 = 0x00; // I/O 使能（功能 2 = 数据）
    pub const IO_1: u32 = 0x01;
    pub const IO_2: u32 = 0x02;
    pub const IO_3: u32 = 0x03;
    pub const IO_4: u32 = 0x04;
    pub const IO_IOE: u32 = 0x02; // I/O 使能寄存器
    pub const IOR: u32 = 0x03; // 就绪
    pub const INT_EN: u32 = 0x04;
    pub const MANUF: u32 = 0x08; // io0 制造商（读）
    pub const PRODUCT: u32 = 0x09; // io0 产品编号
}

/// 回板（backplane）访问寄存器（功能 1；brcmfmac sbsdio 同款）。
#[allow(dead_code)]
pub mod sb {
    pub const FUNC1_SBADDR: u32 = 0x1000; // SB 地址窗口（写 0x18000000|addr）
    pub const PADDR: u32 = 0x1800_0000; // 物理地址映射标志
    pub const CORE_OFFSET_OOB: u32 = 0x1800_0000 | 0x1000; // 样例
}

/// 芯片核心 ID 与复位位（chipcommon）。
#[allow(dead_code)]
pub mod chip {
    pub const CHIPCORE_BASE: u32 = 0x1800_0000;
    pub const RESET_CTL: u32 = 0x01;
    pub const CORE_RESET: u32 = 0x01;
    pub const CORE_RESET_CLK_STOP: u32 = 0x0100;
}

/// SDPCM 帧头偏移（brcmfmac sdio 数据帧；12 字节头）。
#[allow(dead_code)]
pub mod sdpcm {
    pub const HDR_LEN: usize = 12;
    pub const FLAG_POS: usize = 8;
    pub const BCM_BIT_B: u32 = 0x0000_0001; // B = 总线控制/事件
    pub const BCM_BIT_I: u32 = 0x0000_0002; // I = ioctl
    pub const BCM_BIT_A: u32 = 0x0000_0004; // A = async 事件
    pub const BCM_BIT_ACK: u32 = 0x0000_0008;
    pub const BCM_BIT_F: u32 = 0x0000_0010;
    // 真机核对：标志位取值以 brcmfmac 头文件为准（低 16 位）。
}

/// WLC ioctl 命令号（src/shared/wlioctl.h 常用常量；真机接线核对）。
#[allow(dead_code)]
pub mod wlc {
    pub const GET_VERSION: u32 = 1;
    pub const UP: u32 = 2;
    pub const DOWN: u32 = 3;
    pub const SET_INFRA: u32 = 20;
    pub const SET_AUTH: u32 = 22;
    pub const GET_BSSID: u32 = 23;
    pub const SCAN: u32 = 68;
    pub const JOIN: u32 = 70;
    pub const DISASSOC: u32 = 71;
    pub const SCAN_RESULTS: u32 = 74;
    pub const SET_WSEC: u32 = 110;
    pub const SET_WPA_AUTH: u32 = 111;
    pub const GET_RSSI: u32 = 127;
    pub const SET_WSSID: u32 = 118;
}

/// WLC 事件（src/shared/bcmwifi_channels.h / bcmutils 常用常量）。
#[allow(dead_code)]
pub mod wlc_ev {
    pub const SCAN_COMPLETE: u32 = 0;
    pub const SCAN_CONFIRM: u32 = 0;
    pub const LINK: u32 = 6;
    pub const ASSOC: u32 = 13;
    pub const DISASSOC: u32 = 14;
    pub const JOIN: u32 = 33;
    pub const ASSOC_IND: u32 = 12;
    pub const DEAUTH: u32 = 18;
    pub const PSK_SUP: u32 = 46;
}

// ==================== CywWifi 后端状态 ====================

/// 固件 blob（内核从 /boot 读取后注入；须保存在静态/不回收内存中）。
static mut FW_BLOB: Option<&'static [u8]> = None;
/// 探测通过（dwmmc 可访问且 CCCR 读回有效）。
static mut PROBE_OK: bool = false;
/// 固件已注入并完成下载。
static mut FW_LOADED: bool = false;

/// 注入固件 blob（内核侧从 /mnt/fat32 的 brcmfmac43455-sdio.bin 读取）。
pub fn cyw_wifi_set_firmware(blob: &'static [u8]) {
    unsafe { FW_BLOB = Some(blob) };
}

/// CYW43455 SDIO 后端单例（可变引用由 `cyw_wifi` 提供）。
pub static mut CYW_WIFI: CywWifi = CywWifi::new();

/// 获取 CywWifi 可变引用。
pub fn cyw_wifi() -> &'static mut CywWifi {
    unsafe { &mut *core::ptr::addr_of_mut!(CYW_WIFI) }
}

/// CYW43455 SDIO Wi-Fi 后端（骨架；真机接线见 docs/wifi.md）。
pub struct CywWifi {
    mac: [u8; 6],
    link: WifiLinkState,
}

impl CywWifi {
    /// 默认 MAC：由 NVRAM 配置下发（真机上经 WLC GET 读取），骨架用
    /// Raspberry Pi 官方 OUI 预留值。
    pub const fn new() -> Self {
        CywWifi {
            mac: [0xDC, 0xA6, 0x32, 0x00, 0x00, 0x10],
            link: WifiLinkState::Idle,
        }
    }

    /// 探测 SDIO 链路：dwmmc 可访问 + CCCR 读回（功能 0 制造商/产品）。
    /// 这是真机接线第一个可验证点（无需固件）。
    pub fn sdio_probe(&mut self) -> bool {
        if !dw_init() {
            return false;
        }
        // CMD52 读功能 0 的 CCCR 制造商（0x08）与产品（0x09）
        let Some(_manuf) = dw_read8(0, cccr::MANUF) else {
            return false;
        };
        let Some(_prod) = dw_read8(0, cccr::PRODUCT) else {
            return false;
        };
        unsafe { PROBE_OK = true };
        true
    }

    /// 检查后端可用性（探测通过 + 固件已注入加载）。
    pub fn ready(&self) -> bool {
        unsafe { PROBE_OK && FW_LOADED }
    }
}

/// 读取 SDIO 功能寄存器（CMD52 读请求 → CMD52 读数据返回）。
fn dw_read8(func: u8, addr: u32) -> Option<u8> {
    // CMD52 返回数据在 RESP0 低 8 位（真机核对：RESP0[7:0] 或 [15:8]）。
    let resp = dw_send_cmd(52, sdio_cmd52_arg(func, addr, false, 0), true, false)?;
    Some((resp & 0xFF) as u8)
}

// ==================== WifiDevice 实现 ====================

impl WifiDevice for CywWifi {
    fn name(&self) -> &'static str {
        "cyw43455-sdio0"
    }

    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn available(&self) -> bool {
        self.ready()
    }

    fn start_scan(&mut self) -> bool {
        if !self.ready() {
            return false;
        }
        // 接线点：组装 SDPCM I 消息（BCM_BIT_I）发送 WLC_SCAN ioctl，
        // 事件环等待 WLC_E_SCAN_COMPLETE；随后 WLC_SCAN_RESULTS 拉取 BSS 列表。
        // 骨架：链路未接真机前仅记录意图。
        true
    }

    fn poll_scan(&mut self, out: &mut [WifiScanResult]) -> WifiScanPoll {
        let _ = out;
        if !self.ready() {
            return WifiScanPoll { done: true, count: 0 };
        }
        // 接线点：轮询事件环，SCAN_COMPLETE 后解析 WLC_SCAN_RESULTS 的
        // `wlc_ssid_t + bss_info` 结构填充 out；骨架保持 pending。
        WifiScanPoll { done: false, count: 0 }
    }

    fn associate(
        &mut self,
        _bssid: [u8; 6],
        _sta_mac: [u8; 6],
    ) -> Result<(), WifiError> {
        if !self.ready() {
            return Err(WifiError::Unavailable);
        }
        // 接线点：WLC_JOIN ioctl（带 BSSID/SSID）；等待 WLC_E_ASSOC /
        // WLC_E_PSK_SUP（后者为 wsec 卸载模式的消息，本自研握手用
        // 事件环取原始 EAPOL 帧后由内核侧 4-way 完成）。
        // 骨架：握手未接真机前返回 NotImplemented。
        Err(WifiError::NotImplemented)
    }

    fn disassociate(&mut self) {
        self.link = WifiLinkState::Idle;
        // 接线点：WLC_DISASSOC ioctl。
    }

    fn link_state(&self) -> WifiLinkState {
        if !self.ready() {
            return WifiLinkState::Unavailable;
        }
        self.link
    }

    fn recv_eapol(&mut self, _out: &mut [u8]) -> Option<usize> {
        // 接线点：数据路径（功能 2 SDPCM 数据帧，ethertype 0x888E → 0xA0 值）
        // 收帧环弹出。
        None
    }

    fn send_eapol(&mut self, _frame: &[u8]) -> bool {
        if !self.ready() {
            return false;
        }
        // 接线点：SDPCM 数据头（B 消息 + ethertype 帧）写入功能 2 发送环。
        true
    }

    fn status(&self) -> WifiStatus {
        let mut st = WifiStatus::idle();
        st.link = self.link;
        st.bssid = self.mac; // 骨架占位：真实返回关联 AP 的 BSSID
        st
    }
}
