//! 传感器 HAL：加速度计 / 陀螺仪 / 磁力计抽象（I2C 优先，SPI 预留）。
//!
//! 分层设计（硬件 ↔ 总线 ↔ 器件 ↔ 平台）：
//!
//! ```text
//! 硬件（MPU6050/MPU9250 等）
//!   │  I2C（BCM2711 BSC 控制器）或 SPI（预留）
//!   ▼
//! I2cBus trait：BscI2c（Pi4 真实寄存器驱动）/ SimI2c（QEMU/自测模拟）
//!   │
//!   ▼
//! SensorDevice trait：Mpu6050 / Mpu9250（AK8963 磁力计）/ SimSensor
//!   │
//!   ▼
//! Sensor 枚举（无堆分发）→ 内核 /dev/sensor0 → 用户态 sensord
//! ```
//!
//! 采样率配置、量程配置、批处理（FIFO）与低功耗唤醒（wake-on-motion）
//! 均为 [`SensorDevice`] trait 的一等接口；Pi4 无独立传感器 Hub，主 CPU
//! 直接轮询或由 MPU 运动中断唤醒（接线见 `docs/sensors.md`）。

use crate::fusion::Vec3;

// ==================== 错误与基础类型 ====================

/// 传感器错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorError {
    /// I2C 传输失败（NACK / 时钟拉伸超时等）。
    Bus,
    /// 器件未应答（未连接 / 地址错误）。
    NoDevice,
    /// 器件 ID 校验失败。
    BadWhoAmI,
    /// 无效配置。
    InvalidConfig,
    /// 超时。
    Timeout,
}

/// 原始 16 位有符号样本（LSB，器件量程内）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vec3i16 {
    pub x: i16,
    pub y: i16,
    pub z: i16,
}

impl Vec3i16 {
    pub const fn new(x: i16, y: i16, z: i16) -> Self {
        Vec3i16 { x, y, z }
    }
}

/// 一个完整样本（/dev/sensor0 的用户态 ABI，固定 40 字节小端）。
///
/// 布局（repr(C)，8 字节对齐）：`ts_us[0..8] seq[8..12] mag_valid[12]
/// reserved[13..16] accel[16..22] gyro[22..28] mag[28..34]`，尾部 6 字节填充。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SensorSample {
    /// 采样时刻（微秒，内核单调时钟）。
    pub ts_us: u64,
    /// 样本序号（自设备打开递增）。
    pub seq: u32,
    /// 磁力计数据是否有效（0/1）。
    pub mag_valid: u8,
    /// 保留。
    pub reserved: [u8; 3],
    /// 加速度计原始值（LSB）。
    pub accel: Vec3i16,
    /// 陀螺仪原始值（LSB）。
    pub gyro: Vec3i16,
    /// 磁力计原始值（LSB；无磁力计时为 0）。
    pub mag: Vec3i16,
}

impl SensorSample {
    /// ABI 字节长度。
    pub const SIZE: usize = 40;

    /// 序列化为小端字节。
    pub fn to_bytes(&self) -> [u8; SensorSample::SIZE] {
        let mut b = [0u8; SensorSample::SIZE];
        b[0..8].copy_from_slice(&self.ts_us.to_le_bytes());
        b[8..12].copy_from_slice(&self.seq.to_le_bytes());
        b[12] = self.mag_valid;
        b[16..18].copy_from_slice(&self.accel.x.to_le_bytes());
        b[18..20].copy_from_slice(&self.accel.y.to_le_bytes());
        b[20..22].copy_from_slice(&self.accel.z.to_le_bytes());
        b[22..24].copy_from_slice(&self.gyro.x.to_le_bytes());
        b[24..26].copy_from_slice(&self.gyro.y.to_le_bytes());
        b[26..28].copy_from_slice(&self.gyro.z.to_le_bytes());
        b[28..30].copy_from_slice(&self.mag.x.to_le_bytes());
        b[30..32].copy_from_slice(&self.mag.y.to_le_bytes());
        b[32..34].copy_from_slice(&self.mag.z.to_le_bytes());
        b
    }

    /// 从小端字节解析。
    pub fn from_bytes(b: &[u8]) -> Option<SensorSample> {
        if b.len() < SensorSample::SIZE {
            return None;
        }
        let rd = |o: usize| -> i16 { i16::from_le_bytes([b[o], b[o + 1]]) };
        Some(SensorSample {
            ts_us: u64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]),
            seq: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            mag_valid: b[12],
            reserved: [0; 3],
            accel: Vec3i16::new(rd(16), rd(18), rd(20)),
            gyro: Vec3i16::new(rd(22), rd(24), rd(26)),
            mag: Vec3i16::new(rd(28), rd(30), rd(32)),
        })
    }

    /// 用量程灵敏度把原始值换算为物理量。
    /// `accel_lsb_per_g`：加速度 LSB/g；`gyro_lsb_per_dps`：陀螺 LSB/(°/s)。
    pub fn to_physical(&self, accel_lsb_per_g: f32, gyro_lsb_per_dps: f32) -> (Vec3, Vec3, Vec3) {
        let a = Vec3::new(
            self.accel.x as f32 / accel_lsb_per_g,
            self.accel.y as f32 / accel_lsb_per_g,
            self.accel.z as f32 / accel_lsb_per_g,
        );
        let g = Vec3::new(
            self.gyro.x as f32 / gyro_lsb_per_dps,
            self.gyro.y as f32 / gyro_lsb_per_dps,
            self.gyro.z as f32 / gyro_lsb_per_dps,
        );
        let m = if self.mag_valid != 0 {
            Vec3::new(self.mag.x as f32, self.mag.y as f32, self.mag.z as f32)
        } else {
            Vec3::new(0.0, 0.0, 0.0)
        };
        (a, g, m)
    }
}

// ==================== I2C 总线 ====================

/// I2C 总线抽象（主控侧）。
pub trait I2cBus {
    /// 从 `addr` 从机的 `reg` 起连续读 `buf.len()` 字节。
    fn read_regs(&mut self, addr: u8, reg: u8, buf: &mut [u8]) -> Result<(), SensorError>;
    /// 向 `addr` 从机的 `reg` 写 1 字节。
    fn write_reg(&mut self, addr: u8, reg: u8, data: u8) -> Result<(), SensorError>;
    /// 探测从机是否存在（发送空写事务 / 读 WHO_AM_I）。
    fn probe(&mut self, addr: u8) -> bool;
    /// 传输方式名（日志用）。
    fn name(&self) -> &str;
}

/// 便利方法：读 1 字节。
pub trait I2cBusExt: I2cBus {
    fn read_reg(&mut self, addr: u8, reg: u8) -> Result<u8, SensorError> {
        let mut b = [0u8; 1];
        self.read_regs(addr, reg, &mut b)?;
        Ok(b[0])
    }
}
impl<T: I2cBus> I2cBusExt for T {}

// ==================== BCM2711 BSC（I2C）控制器 ====================
//
// Pi4 的 I2C1（40 针排针 GPIO 2/3）由 BSC1 控制器驱动，寄存器布局与
// BCM2835 一致（BCM2711 外设基址 0xFE000000 + BSC1 偏移 0x804000）。
// 参考 Linux drivers/i2c/busses/i2c-bcm2835.c。

/// BCM2711 BSC1 控制器基址（I2C1，引脚 GPIO 2=SDA / 3=SCL）。
pub const BSC1_BASE: usize = 0xFE80_4000;
/// BSC1 中断号（GIC SPI 109 + 32；当前轮询方式未使用，预留）。
pub const BSC1_IRQ: u32 = 141;
/// BCM2711 外设 GPIO 基址（GPFSEL0 在此 +0，GPPUPPDN0 在此 +0xE4）。
pub const GPIO_BASE: usize = 0xFE20_0000;

#[repr(C)]
struct BscRegs {
    c: u32,      // 0x00 Control
    s: u32,      // 0x04 Status
    dlen: u32,   // 0x08 Data length
    a: u32,      // 0x0C Slave address
    fifo: u32,   // 0x10 FIFO
    div: u32,    // 0x14 Clock divider
    del: u32,    // 0x18 Data delay
    clkt: u32,   // 0x1C Clock stretch timeout
}

// 控制寄存器位。
const C_I2CEN: u32 = 1 << 15;
#[allow(dead_code)] // 中断模式预留（当前轮询）
const C_INTR: u32 = 1 << 10;
#[allow(dead_code)]
const C_INTD: u32 = 1 << 9;
const C_ST: u32 = 1 << 7;
const C_CLEAR1: u32 = 1 << 5;
const C_CLEAR2: u32 = 1 << 4;
const C_READ: u32 = 1 << 0;
// 状态寄存器位（BCM2835/2711 手册 S 寄存器：bit2=TXW、bit3=RXR、
// bit4=TXD、bit5=RXD、bit6=TXE、bit7=RXF —— 此前整体错位成 5/6/7，
// 写路径 wait_for(S_TXW) 实际在等 RXD，FIFO 清空后恒 0 → I2C 写必超时）。
const S_CLKT: u32 = 1 << 9;
const S_ERR: u32 = 1 << 8;
#[allow(dead_code)] // 中断模式预留
const S_TXD: u32 = 1 << 4; // FIFO 可接受数据（写前等待）
const S_RXR: u32 = 1 << 3; // FIFO 需要读取（读中等待）
const S_TXW: u32 = 1 << 2; // FIFO 需要写入（写满等待）
const S_DONE: u32 = 1 << 1;
const S_TA: u32 = 1 << 0;
const S_CLEAR_MASK: u32 = S_CLKT | S_ERR | S_DONE | S_TA;

/// BCM2711 BSC（I2C）控制器驱动。
pub struct BscI2c {
    base: usize,
    clock_hz: u32,
    baud: u32,
}

impl BscI2c {
    /// 创建 BSC1 控制器驱动（默认 100kHz）。
    pub const fn new(base: usize, clock_hz: u32) -> Self {
        BscI2c { base, clock_hz, baud: 100_000 }
    }

    /// 配置时钟分频：`div = clock / (baud * 2)`。
    fn setup_div(&mut self) {
        let div = (self.clock_hz / (self.baud * 2)).clamp(1, 65535) as u32;
        unsafe {
            let regs = self.base as *mut BscRegs;
            core::ptr::write_volatile(&mut (*regs).div, div);
        }
    }

    /// 清除 FIFO 与错误状态。
    fn clear(&mut self) {
        unsafe {
            let regs = self.base as *mut BscRegs;
            let s = core::ptr::read_volatile(&(*regs).s);
            core::ptr::write_volatile(&mut (*regs).s, s & !S_CLEAR_MASK);
            let mut c = core::ptr::read_volatile(&(*regs).c);
            c |= C_CLEAR1 | C_CLEAR2;
            core::ptr::write_volatile(&mut (*regs).c, c);
            // CLEAR 位写 1 后硬件自动清零；等待其回落。
            for _ in 0..100 {
                let c2 = core::ptr::read_volatile(&(*regs).c);
                if c2 & (C_CLEAR1 | C_CLEAR2) == 0 {
                    break;
                }
            }
        }
    }

    /// 等待状态位置位（或超时）。
    fn wait_for(&mut self, mask: u32, timeout: u32) -> Result<(), SensorError> {
        unsafe {
            let regs = self.base as *mut BscRegs;
            for _ in 0..timeout {
                let s = core::ptr::read_volatile(&(*regs).s);
                if s & mask != 0 {
                    if s & (S_ERR | S_CLKT) != 0 {
                        return Err(SensorError::Bus);
                    }
                    return Ok(());
                }
                core::hint::spin_loop();
            }
        }
        Err(SensorError::Timeout)
    }

    /// 执行一次传输（`read = false` 写 / `true` 读）。
    fn transfer(&mut self, addr: u8, read: bool, data: &mut [u8]) -> Result<(), SensorError> {
        unsafe {
            let regs = self.base as *mut BscRegs;
            // 每次传输前确保时钟分频已配置（幂等，首次生效）。
            self.setup_div();
            self.clear();
            core::ptr::write_volatile(&mut (*regs).a, addr as u32);
            core::ptr::write_volatile(&mut (*regs).dlen, data.len() as u32);
            let mut c = C_I2CEN;
            if read {
                c |= C_READ;
            }
            core::ptr::write_volatile(&mut (*regs).c, c);
            if !read {
                for &b in data.iter() {
                    // 等待 TX FIFO 有空位。
                    self.wait_for(S_TXW, 1000)?;
                    core::ptr::write_volatile(&mut (*regs).fifo, b as u32);
                }
            }
            // 启动传输。
            c |= C_ST;
            core::ptr::write_volatile(&mut (*regs).c, c);
            self.wait_for(S_DONE, 10_000)?;
            if read {
                for b in data.iter_mut() {
                    // 等待 RX FIFO 有数据。
                    self.wait_for(S_RXR, 1000)?;
                    *b = core::ptr::read_volatile(&(*regs).fifo) as u8;
                }
            }
            Ok(())
        }
    }
}

impl I2cBus for BscI2c {
    fn read_regs(&mut self, addr: u8, reg: u8, buf: &mut [u8]) -> Result<(), SensorError> {
        // 先写寄存器地址，再（repeated start）读数据。
        let mut reg_buf = [reg];
        self.transfer(addr, false, &mut reg_buf)?;
        self.transfer(addr, true, buf)
    }

    fn write_reg(&mut self, addr: u8, reg: u8, data: u8) -> Result<(), SensorError> {
        let mut tx = [reg, data];
        self.transfer(addr, false, &mut tx)
    }

    fn probe(&mut self, addr: u8) -> bool {
        // 空写事务：被 NACK 或超时视为不存在。
        let mut empty: [u8; 0] = [];
        match self.transfer(addr, false, &mut empty) {
            Ok(()) => true,
            Err(_) => false,
        }
    }

    fn name(&self) -> &str {
        "bcm2711-bsc"
    }
}

/// 启用 Pi4 I2C1 引脚（GPIO 2/3 → ALT0）与上拉电阻。
///
/// 注：多数 Pi 扩展板自带 I2C1 外部上拉；此处仍显式配置 GPIO 功能与
/// 内部上拉，保证裸机下不依赖固件默认状态。
pub fn bsc1_gpio_init() {
    unsafe {
        // GPFSEL0：GPIO2 位 [8:6]、GPIO3 位 [11:9] → ALT0 (0b100)。
        let fsel0 = GPIO_BASE as *mut u32;
        let mut v = core::ptr::read_volatile(fsel0);
        v &= !(0b111 << 6);
        v |= 0b100 << 6;
        v &= !(0b111 << 9);
        v |= 0b100 << 9;
        core::ptr::write_volatile(fsel0, v);
        // GPPUPPDN0（BCM2711）：每 GPIO 2 位，01 = 上拉。
        // GPIO2 位 [5:4]、GPIO3 位 [7:6]。
        let puppdn0 = (GPIO_BASE + 0xE4) as *mut u32;
        let mut p = core::ptr::read_volatile(puppdn0);
        p &= !(0b11 << 4);
        p |= 0b01 << 4;
        p &= !(0b11 << 6);
        p |= 0b01 << 6;
        core::ptr::write_volatile(puppdn0, p);
    }
}

// ==================== 模拟 I2C 总线（自测 / 无硬件环境） ====================

/// 模拟 I2C 总线：寄存器文件风格的 MPU6050 模型。
///
/// 用于 `cargo test` 自测驱动路径：测试可先 `set_gyro_raw/set_accel_raw`
/// 预置数据，再经 `Mpu6050<SimI2c>` 读取，验证寄存器地址与换算正确。
pub struct SimI2c {
    /// 寄存器文件（128 字节，MPU6050 布局）。
    regs: [u8; 128],
}

impl SimI2c {
    pub fn new() -> Self {
        let mut regs = [0u8; 128];
        regs[0x75] = 0x68; // WHO_AM_I
        SimI2c { regs }
    }

    /// 预置加速度原始值（LSB）。
    pub fn set_accel_raw(&mut self, x: i16, y: i16, z: i16) {
        self.regs[0x3B..0x41].copy_from_slice(&raw6(x, y, z));
    }

    /// 预置陀螺原始值（LSB）。
    pub fn set_gyro_raw(&mut self, x: i16, y: i16, z: i16) {
        self.regs[0x43..0x49].copy_from_slice(&raw6(x, y, z));
    }
}

impl Default for SimI2c {
    fn default() -> Self {
        Self::new()
    }
}

/// 3 个 i16 → 6 字节小端。
fn raw6(x: i16, y: i16, z: i16) -> [u8; 6] {
    let mut b = [0u8; 6];
    b[0..2].copy_from_slice(&x.to_le_bytes());
    b[2..4].copy_from_slice(&y.to_le_bytes());
    b[4..6].copy_from_slice(&z.to_le_bytes());
    b
}

impl I2cBus for SimI2c {
    fn read_regs(&mut self, addr: u8, reg: u8, buf: &mut [u8]) -> Result<(), SensorError> {
        if !matches!(addr, 0x68 | 0x69 | 0x0C) {
            return Err(SensorError::NoDevice);
        }
        let n = buf.len();
        if reg as usize + n > 128 {
            return Err(SensorError::InvalidConfig);
        }
        buf.copy_from_slice(&self.regs[reg as usize..reg as usize + n]);
        Ok(())
    }

    fn write_reg(&mut self, addr: u8, reg: u8, data: u8) -> Result<(), SensorError> {
        if !matches!(addr, 0x68 | 0x69 | 0x0C) {
            return Err(SensorError::NoDevice);
        }
        self.regs[reg as usize] = data;
        Ok(())
    }

    fn probe(&mut self, addr: u8) -> bool {
        // 模拟总线上的已知器件：MPU6050/9250 (0x68/0x69) 与 AK8963 (0x0C)。
        addr == 0x68 || addr == 0x69 || addr == 0x0C
    }

    fn name(&self) -> &str {
        "sim-i2c"
    }
}

// ==================== 器件配置 ====================

/// 加速度计量程。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccelRange {
    /// ±2 g（默认）
    G2 = 0,
    G4 = 1,
    G8 = 2,
    G16 = 3,
}

/// 陀螺仪量程。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GyroRange {
    /// ±250 dps（默认）
    Dps250 = 0,
    Dps500 = 1,
    Dps1000 = 2,
    Dps2000 = 3,
}

/// 加速度 LSB/g（按量程）。
pub fn accel_lsb_per_g(r: AccelRange) -> f32 {
    match r {
        AccelRange::G2 => 16384.0,
        AccelRange::G4 => 8192.0,
        AccelRange::G8 => 4096.0,
        AccelRange::G16 => 2048.0,
    }
}

/// 陀螺 LSB/(°/s)。
pub fn gyro_lsb_per_dps(r: GyroRange) -> f32 {
    match r {
        GyroRange::Dps250 => 131.0,
        GyroRange::Dps500 => 65.5,
        GyroRange::Dps1000 => 32.8,
        GyroRange::Dps2000 => 16.4,
    }
}

/// 传感器配置。
#[derive(Clone, Copy, Debug)]
pub struct SensorConfig {
    /// 目标采样率（Hz）。真实器件：DLPF 开启时内部 1kHz，
    /// `SMPLRT_DIV = 1000/rate - 1`；模拟器件：每样本推进 1/rate 秒。
    pub sample_rate_hz: u32,
    pub accel_range: AccelRange,
    pub gyro_range: GyroRange,
}

impl Default for SensorConfig {
    fn default() -> Self {
        SensorConfig {
            sample_rate_hz: 100,
            accel_range: AccelRange::G2,
            gyro_range: GyroRange::Dps250,
        }
    }
}

/// 传感器来源（平台后端返回）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorSource {
    /// 软件模拟（QEMU / 无硬件回退）。
    Sim,
    /// MPU6050（加速度 + 陀螺）。
    Mpu6050,
    /// MPU9250（加速度 + 陀螺 + AK8963 磁力计）。
    Mpu9250,
}

/// 平台返回的传感器参数。
#[derive(Clone, Copy, Debug)]
pub struct SensorInfo {
    pub source: SensorSource,
    /// I2C 控制器 MMIO 基址（真实器件）。
    pub i2c_base: usize,
    /// I2C 中断号（预留，当前轮询）。
    pub i2c_irq: u32,
    /// I2C 控制器时钟（Hz）。
    pub i2c_clock_hz: u32,
    /// IMU I2C 从机地址（MPU6050/9250 = 0x68/0x69）。
    pub slave_addr: u8,
    /// 磁力计从机地址（AK8963 = 0x0C）。
    pub mag_slave_addr: u8,
}

impl SensorInfo {
    pub const fn new(
        source: SensorSource,
        i2c_base: usize,
        i2c_irq: u32,
        i2c_clock_hz: u32,
        slave_addr: u8,
        mag_slave_addr: u8,
    ) -> Self {
        SensorInfo {
            source,
            i2c_base,
            i2c_irq,
            i2c_clock_hz,
            slave_addr,
            mag_slave_addr,
        }
    }

    /// 软件模拟传感器参数（QEMU 平台）。
    pub const fn simulated() -> Self {
        SensorInfo {
            source: SensorSource::Sim,
            i2c_base: 0,
            i2c_irq: 0,
            i2c_clock_hz: 0,
            slave_addr: 0,
            mag_slave_addr: 0,
        }
    }
}

// ==================== 器件层：MPU6050 ====================

/// MPU6050 寄存器。
pub mod mpu_regs {
    pub const WHO_AM_I: u8 = 0x75;
    pub const PWR_MGMT_1: u8 = 0x6B;
    pub const SMPLRT_DIV: u8 = 0x19;
    pub const CONFIG: u8 = 0x1A;
    pub const GYRO_CONFIG: u8 = 0x1B;
    pub const ACCEL_CONFIG: u8 = 0x1C;
    pub const ACCEL_XOUT_H: u8 = 0x3B;
    pub const GYRO_XOUT_H: u8 = 0x43;
    pub const FIFO_EN: u8 = 0x23;
    pub const INT_ENABLE: u8 = 0x38;
    pub const INT_STATUS: u8 = 0x3A;
    pub const MOT_THR: u8 = 0x1F;
    pub const MOT_DUR: u8 = 0x20;
    pub const FIFO_COUNT_H: u8 = 0x72;
}

/// MPU6050 六轴 IMU 驱动（I2C）。
pub struct Mpu6050<I: I2cBus> {
    pub i2c: I,
    pub addr: u8,
    accel_range: AccelRange,
    gyro_range: GyroRange,
    sample_rate: u32,
    have_mag: bool,
}

impl<I: I2cBus> Mpu6050<I> {
    pub fn new(i2c: I, addr: u8) -> Self {
        Mpu6050 {
            i2c,
            addr,
            accel_range: AccelRange::G2,
            gyro_range: GyroRange::Dps250,
            sample_rate: 100,
            have_mag: false,
        }
    }

    /// 读取 6 字节原始轴数据。
    fn read_axes(&mut self, reg: u8) -> Result<Vec3i16, SensorError> {
        let mut b = [0u8; 6];
        self.i2c.read_regs(self.addr, reg, &mut b)?;
        Ok(Vec3i16::new(
            i16::from_le_bytes([b[0], b[1]]),
            i16::from_le_bytes([b[2], b[3]]),
            i16::from_le_bytes([b[4], b[5]]),
        ))
    }
}

impl<I: I2cBus> SensorDevice for Mpu6050<I> {
    fn whoami(&mut self) -> Result<u8, SensorError> {
        self.i2c.read_reg(self.addr, mpu_regs::WHO_AM_I)
    }

    fn probe(&mut self) -> Result<bool, SensorError> {
        if !self.i2c.probe(self.addr) {
            return Ok(false);
        }
        let id = self.whoami()?;
        Ok(id == 0x68 || id == 0x70 || id == 0x71) // 6050/6500/9250 家族
    }

    fn reset(&mut self) -> Result<(), SensorError> {
        // DEVICE_RESET。
        self.i2c.write_reg(self.addr, mpu_regs::PWR_MGMT_1, 0x80)?;
        // 等待复位完成（模拟总线立即完成；真实总线简单忙等）。
        for _ in 0..100 {
            let v = self.i2c.read_reg(self.addr, mpu_regs::PWR_MGMT_1)?;
            if v & 0x80 == 0 {
                break;
            }
        }
        // 退出休眠，时钟源 = PLL X 轴陀螺。
        self.i2c.write_reg(self.addr, mpu_regs::PWR_MGMT_1, 0x01)
    }

    fn configure(&mut self, cfg: &SensorConfig) -> Result<(), SensorError> {
        self.sample_rate = cfg.sample_rate_hz.clamp(1, 1000);
        self.accel_range = cfg.accel_range;
        self.gyro_range = cfg.gyro_range;
        // DLPF：开启 42Hz 低通（CONFIG[2:0]=0x03），内部采样 1kHz。
        self.i2c.write_reg(self.addr, mpu_regs::CONFIG, 0x03)?;
        // 采样率分频：rate = 1000 / (1 + SMPLRT_DIV)。
        let div = (1000u32 / self.sample_rate).saturating_sub(1).min(255) as u8;
        self.i2c.write_reg(self.addr, mpu_regs::SMPLRT_DIV, div)?;
        // 量程。
        self.i2c
            .write_reg(self.addr, mpu_regs::ACCEL_CONFIG, (self.accel_range as u8) << 3)?;
        self.i2c
            .write_reg(self.addr, mpu_regs::GYRO_CONFIG, (self.gyro_range as u8) << 3)?;
        Ok(())
    }

    fn read_sample(&mut self) -> Result<SensorSample, SensorError> {
        let accel = self.read_axes(mpu_regs::ACCEL_XOUT_H)?;
        let gyro = self.read_axes(mpu_regs::GYRO_XOUT_H)?;
        Ok(SensorSample {
            seq: 0, // 由内核设备层填充
            ts_us: 0,
            accel,
            gyro,
            mag: Vec3i16::new(0, 0, 0),
            mag_valid: 0,
            reserved: [0; 3],
        })
    }

    fn accel_lsb_per_g(&self) -> f32 {
        accel_lsb_per_g(self.accel_range)
    }
    fn gyro_lsb_per_dps(&self) -> f32 {
        gyro_lsb_per_dps(self.gyro_range)
    }
    fn have_mag(&self) -> bool {
        self.have_mag
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn name(&self) -> &str {
        "mpu6050"
    }

    fn set_batch(&mut self, enabled: bool, _max_samples: u8) -> Result<(), SensorError> {
        // FIFO 批处理预留：FIFO_EN（0x23）ACCEL/GYRO 位 + FIFO_COUNT 读取。
        let v = if enabled { 0x08 | 0x10 } else { 0 }; // ACCEL_FIFO_EN | GYRO_FIFO_EN
        self.i2c.write_reg(self.addr, mpu_regs::FIFO_EN, v)
    }

    fn fifo_count(&mut self) -> Result<u16, SensorError> {
        let hi = self.i2c.read_reg(self.addr, mpu_regs::FIFO_COUNT_H)? as u16;
        let lo = self.i2c.read_reg(self.addr, mpu_regs::FIFO_COUNT_H + 1)? as u16;
        Ok((hi << 8) | lo)
    }

    fn enable_wake_on_motion(&mut self, threshold: u8, duration: u8) -> Result<(), SensorError> {
        // 低功耗运动唤醒：MOT_THR / MOT_DUR / INT_ENABLE(WMOT) / PWR_MGMT_1(CYCLE)。
        self.i2c.write_reg(self.addr, mpu_regs::MOT_THR, threshold)?;
        self.i2c.write_reg(self.addr, mpu_regs::MOT_DUR, duration)?;
        // INT_ENABLE bit6 = MOT_EN；PWR_MGMT_1 bit5 = CYCLE（循环采样）。
        self.i2c.write_reg(self.addr, mpu_regs::INT_ENABLE, 0x40)?;
        self.i2c.write_reg(self.addr, mpu_regs::PWR_MGMT_1, 0x21)
    }
}

// ==================== 器件层：MPU9250（含 AK8963 磁力计） ====================

/// AK8963（MPU9250 内置）磁力计寄存器。
pub mod ak8963_regs {
    pub const WIA: u8 = 0x00;
    pub const ST1: u8 = 0x02;
    pub const HXL: u8 = 0x03;
    pub const CNTL1: u8 = 0x0A;
    pub const ASAX: u8 = 0x10;
}

/// MPU9250 九轴 IMU（MPU6050 内核 + AK8963 磁力计）。
pub struct Mpu9250<I: I2cBus> {
    pub imu: Mpu6050<I>,
    pub mag_addr: u8,
    /// AK8963 灵敏度调整（ASAX 3 字节，出厂校准）。
    adjust: [u8; 3],
}

impl<I: I2cBus> Mpu9250<I> {
    pub fn new(i2c: I, addr: u8, mag_addr: u8) -> Self {
        Mpu9250 {
            imu: Mpu6050::new(i2c, addr),
            mag_addr,
            adjust: [0; 3],
        }
    }

    /// 配置磁力计：连续测量 100Hz（CNTL1 = 0x16）。
    fn mag_init(&mut self) -> Result<(), SensorError> {
        // 先复位。
        self.imu.i2c.write_reg(self.mag_addr, ak8963_regs::CNTL1, 0x00)?;
        // 读取灵敏度调整。
        self.imu
            .i2c
            .read_regs(self.mag_addr, ak8963_regs::ASAX, &mut self.adjust)?;
        // 连续测量模式 2（100Hz）。
        self.imu.i2c.write_reg(self.mag_addr, ak8963_regs::CNTL1, 0x16)?;
        Ok(())
    }

    /// 读磁力计（16 位，0.15 µT/LSB，含灵敏度调整）。
    fn read_mag_raw(&mut self) -> Result<Vec3i16, SensorError> {
        // 等待 DRDY（ST1 bit0）。
        for _ in 0..100 {
            let st = self.imu.i2c.read_reg(self.mag_addr, ak8963_regs::ST1)?;
            if st & 0x01 != 0 {
                break;
            }
        }
        let mut b = [0u8; 6];
        self.imu
            .i2c
            .read_regs(self.mag_addr, ak8963_regs::HXL, &mut b)?;
        let mut raw = Vec3i16::new(
            i16::from_le_bytes([b[0], b[1]]),
            i16::from_le_bytes([b[2], b[3]]),
            i16::from_le_bytes([b[4], b[5]]),
        );
        // 灵敏度调整：val = raw * (adj + 128) / 256。
        let adj = |i: usize| -> i16 {
            let a = self.adjust[i] as i16;
            (a + 128) as i16
        };
        raw.x = ((raw.x as i32 * adj(0) as i32) / 256) as i16;
        raw.y = ((raw.y as i32 * adj(1) as i32) / 256) as i16;
        raw.z = ((raw.z as i32 * adj(2) as i32) / 256) as i16;
        Ok(raw)
    }
}

impl<I: I2cBus> SensorDevice for Mpu9250<I> {
    fn whoami(&mut self) -> Result<u8, SensorError> {
        self.imu.whoami()
    }

    fn probe(&mut self) -> Result<bool, SensorError> {
        if !self.imu.i2c.probe(self.imu.addr) {
            return Ok(false);
        }
        let id = self.imu.whoami()?;
        let imu_ok = id == 0x71 || id == 0x68;
        let mag_ok = self.imu.i2c.probe(self.mag_addr);
        Ok(imu_ok && mag_ok)
    }

    fn reset(&mut self) -> Result<(), SensorError> {
        self.imu.reset()
    }

    fn configure(&mut self, cfg: &SensorConfig) -> Result<(), SensorError> {
        self.imu.configure(cfg)?;
        self.mag_init()
    }

    fn read_sample(&mut self) -> Result<SensorSample, SensorError> {
        let accel = self.imu.read_axes(mpu_regs::ACCEL_XOUT_H)?;
        let gyro = self.imu.read_axes(mpu_regs::GYRO_XOUT_H)?;
        let mag = self.read_mag_raw()?;
        Ok(SensorSample {
            seq: 0,
            ts_us: 0,
            accel,
            gyro,
            mag,
            mag_valid: 1,
            reserved: [0; 3],
        })
    }

    fn accel_lsb_per_g(&self) -> f32 {
        self.imu.accel_lsb_per_g()
    }
    fn gyro_lsb_per_dps(&self) -> f32 {
        self.imu.gyro_lsb_per_dps()
    }
    fn have_mag(&self) -> bool {
        true
    }
    fn sample_rate(&self) -> u32 {
        self.imu.sample_rate()
    }
    fn name(&self) -> &str {
        "mpu9250"
    }

    fn set_batch(&mut self, enabled: bool, max_samples: u8) -> Result<(), SensorError> {
        self.imu.set_batch(enabled, max_samples)
    }

    fn fifo_count(&mut self) -> Result<u16, SensorError> {
        self.imu.fifo_count()
    }

    fn enable_wake_on_motion(&mut self, threshold: u8, duration: u8) -> Result<(), SensorError> {
        self.imu.enable_wake_on_motion(threshold, duration)
    }
}

// ==================== 模拟传感器（QEMU / 无硬件回退） ====================

/// 模拟传感器剧本。
///
/// 确定性脚本（与真实时间解耦，按采样率推进仿真时钟）：
/// - `t < 2s`：平放桌面（roll=pitch=yaw=0），轻微噪声
/// - `2s ≤ t < 3s`：拿起设备（roll 0→90°，抬腕亮屏触发点）
/// - `3s ≤ t < 8s`：竖持并绕 Z 轴扫过 360°（航向/自动旋转演示）
/// - `8s ≤ t < 9s`：放回桌面（roll 90°→0）
/// - `t ≥ 9s`：平放 + 每 0.6s 一次行走颠簸（计步演示）
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SimMode {
    Script,
    /// 固定姿态（度），用于确定性测试。
    Fixed { roll: f32, pitch: f32, yaw: f32 },
    /// 绕 Z 匀速旋转（度/秒）。
    Sweep { dps: f32 },
}

/// 软件模拟 IMU。
pub struct SimSensor {
    mode: SimMode,
    sample_rate: u32,
    have_mag: bool,
    t: f32,
    seq: u32,
    roll: f32,
    pitch: f32,
    yaw: f32,
    rng: u32,
    // 缓存的物理量。
    accel: Vec3,
    gyro: Vec3,
    mag: Vec3,
}

impl SimSensor {
    pub fn new() -> Self {
        SimSensor {
            mode: SimMode::Script,
            sample_rate: 100,
            have_mag: true,
            t: 0.0,
            seq: 0,
            roll: 0.0,
            pitch: 0.0,
            yaw: 0.0,
            rng: 0x9E37_79B9,
            accel: Vec3::new(0.0, 0.0, 1.0),
            gyro: Vec3::new(0.0, 0.0, 0.0),
            mag: Vec3::new(0.4, 0.0, 0.6),
        }
    }

    pub fn set_mode(&mut self, mode: SimMode) {
        self.mode = mode;
        self.t = 0.0;
        self.roll = 0.0;
        self.pitch = 0.0;
        self.yaw = 0.0;
    }

    /// 确定性噪声（LCG，±`amp` 范围）。
    fn noise(&mut self, amp: f32) -> f32 {
        self.rng = self.rng.wrapping_mul(1664525).wrapping_add(1013904223);
        let u = ((self.rng >> 8) & 0xFFFF) as f32 / 65535.0; // [0,1]
        (u * 2.0 - 1.0) * amp
    }

    /// 按剧本推进姿态，返回 (roll, pitch, yaw)（度，不含噪声）。
    fn script_pose(&mut self) -> (f32, f32, f32) {
        let t = self.t;
        match self.mode {
            SimMode::Script => {
                let r = if t < 2.0 {
                    0.0
                } else if t < 3.0 {
                    90.0 * (t - 2.0)
                } else if t < 8.0 {
                    90.0
                } else if t < 9.0 {
                    90.0 * (9.0 - t)
                } else {
                    0.0
                };
                let p = if (3.0..8.0).contains(&t) {
                    10.0 * crate::math::sin((t - 3.0) * 2.0 * core::f32::consts::PI / 1.2)
                } else {
                    0.0
                };
                let y = if (3.0..8.0).contains(&t) {
                    (t - 3.0) * 72.0
                } else {
                    0.0
                };
                (r, p, y)
            }
            SimMode::Fixed { roll, pitch, yaw } => (roll, pitch, yaw),
            SimMode::Sweep { dps } => (0.0, 0.0, t * dps),
        }
    }

    /// 推进一个采样周期：更新姿态与物理量。
    fn tick(&mut self) {
        let dt = 1.0 / self.sample_rate.max(1) as f32;
        self.t += dt;
        self.seq = self.seq.wrapping_add(1);

        // 确定性姿态（不含噪声）——陀螺由姿态差分计算，噪声单独施加，
        // 避免把姿态噪声经 dt 放大成虚假角速度。
        let (old_r, old_p, old_y) = (self.roll, self.pitch, self.yaw);
        let (r, p, y) = self.script_pose();
        self.roll = r;
        self.pitch = p;
        self.yaw = y;

        // 角速度 = 姿态差分 / dt（体坐标系近似，对仿真足够）。
        let mut gyro = Vec3::new(
            (r - old_r) / dt,
            (p - old_p) / dt,
            (y - old_y) / dt,
        );
        gyro.x += self.noise(0.05);
        gyro.y += self.noise(0.05);
        gyro.z += self.noise(0.05);

        // 重力在体坐标系的分量（静止时加速度计读数）。
        let q = crate::fusion::euler_to_quat(r, p, y);
        let mut accel = crate::fusion::gravity_in_body(q);
        accel.x += self.noise(0.005);
        accel.y += self.noise(0.005);
        accel.z += self.noise(0.005);

        // 行走颠簸：t ≥ 9s 后每 0.6s 一次 0.4g 的 z 向冲击（计步演示）。
        if let SimMode::Script = self.mode {
            if self.t >= 9.0 {
                let phase = crate::math::rem_euclid(self.t - 9.0, 0.6);
                if phase < 0.15 {
                    let k = crate::math::sin(phase / 0.15 * core::f32::consts::PI);
                    accel.z += 0.4 * k;
                    accel.y += 0.15 * k;
                }
            }
        }

        // 磁场（体坐标系）：世界系 (0.4, 0, 0.6) 旋转到体。
        let mag = q.rotate_inv(Vec3::new(0.4, 0.0, 0.6));

        self.gyro = gyro;
        self.accel = accel;
        self.mag = mag;
    }
}

impl Default for SimSensor {
    fn default() -> Self {
        Self::new()
    }
}

impl SensorDevice for SimSensor {
    fn whoami(&mut self) -> Result<u8, SensorError> {
        Ok(0x53) // 模拟 ID
    }

    fn probe(&mut self) -> Result<bool, SensorError> {
        Ok(true)
    }

    fn reset(&mut self) -> Result<(), SensorError> {
        Ok(())
    }

    fn configure(&mut self, cfg: &SensorConfig) -> Result<(), SensorError> {
        self.sample_rate = cfg.sample_rate_hz.clamp(1, 1000);
        Ok(())
    }

    fn read_sample(&mut self) -> Result<SensorSample, SensorError> {
        self.tick();
        let accel_lsb = accel_lsb_per_g(AccelRange::G2);
        let gyro_lsb = gyro_lsb_per_dps(GyroRange::Dps250);
        Ok(SensorSample {
            seq: self.seq,
            ts_us: 0, // 由内核设备层填充
            accel: Vec3i16::new(
                (self.accel.x * accel_lsb) as i16,
                (self.accel.y * accel_lsb) as i16,
                (self.accel.z * accel_lsb) as i16,
            ),
            gyro: Vec3i16::new(
                (self.gyro.x * gyro_lsb) as i16,
                (self.gyro.y * gyro_lsb) as i16,
                (self.gyro.z * gyro_lsb) as i16,
            ),
            mag: Vec3i16::new(
                (self.mag.x * 6600.0) as i16, // 0.15µT/LSB 下的 16 位满量程近似
                (self.mag.y * 6600.0) as i16,
                (self.mag.z * 6600.0) as i16,
            ),
            mag_valid: if self.have_mag { 1 } else { 0 },
            reserved: [0; 3],
        })
    }

    fn accel_lsb_per_g(&self) -> f32 {
        accel_lsb_per_g(AccelRange::G2)
    }
    fn gyro_lsb_per_dps(&self) -> f32 {
        gyro_lsb_per_dps(GyroRange::Dps250)
    }
    fn have_mag(&self) -> bool {
        self.have_mag
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn name(&self) -> &str {
        "sim-imu"
    }

    fn set_batch(&mut self, _enabled: bool, _max_samples: u8) -> Result<(), SensorError> {
        Ok(())
    }
    fn fifo_count(&mut self) -> Result<u16, SensorError> {
        Ok(0)
    }
    fn enable_wake_on_motion(&mut self, _threshold: u8, _duration: u8) -> Result<(), SensorError> {
        Ok(())
    }
}

// ==================== 器件 trait 与统一分发 ====================

/// 传感器器件抽象（采样率配置、批处理预留、低功耗唤醒）。
pub trait SensorDevice {
    /// 读 WHO_AM_I。
    fn whoami(&mut self) -> Result<u8, SensorError>;
    /// 探测器件是否存在。
    fn probe(&mut self) -> Result<bool, SensorError>;
    /// 复位（退出休眠）。
    fn reset(&mut self) -> Result<(), SensorError>;
    /// 配置采样率 / 量程。
    fn configure(&mut self, cfg: &SensorConfig) -> Result<(), SensorError>;
    /// 读取一个完整样本（加速度 + 陀螺 [+ 磁]），推进仿真时钟。
    fn read_sample(&mut self) -> Result<SensorSample, SensorError>;
    /// 加速度灵敏度（LSB/g）。
    fn accel_lsb_per_g(&self) -> f32;
    /// 陀螺灵敏度（LSB/(°/s)）。
    fn gyro_lsb_per_dps(&self) -> f32;
    /// 是否有磁力计。
    fn have_mag(&self) -> bool;
    /// 当前采样率。
    fn sample_rate(&self) -> u32;
    /// 器件名（日志）。
    fn name(&self) -> &str;
    /// 批处理模式（FIFO）——预留接口。
    fn set_batch(&mut self, enabled: bool, max_samples: u8) -> Result<(), SensorError>;
    /// FIFO 中样本数——预留接口。
    fn fifo_count(&mut self) -> Result<u16, SensorError>;
    /// 低功耗运动唤醒——预留接口（Pi4 无传感器 Hub，主 CPU 直接处理）。
    fn enable_wake_on_motion(&mut self, threshold: u8, duration: u8) -> Result<(), SensorError>;
}

/// 统一传感器（无堆枚举分发：内核与用户态共用同一抽象）。
pub enum Sensor {
    Sim(SimSensor),
    Mpu6050Bsc(Mpu6050<BscI2c>),
    Mpu9250Bsc(Mpu9250<BscI2c>),
    #[allow(dead_code)]
    Mpu6050Sim(Mpu6050<SimI2c>),
}

impl Sensor {
    /// 按平台信息打开传感器；真实器件探测失败时回退到模拟器。
    pub fn open(info: &SensorInfo) -> Result<Sensor, SensorError> {
        match info.source {
            SensorSource::Sim => Ok(Sensor::Sim(SimSensor::new())),
            SensorSource::Mpu6050 => {
                let i2c = BscI2c::new(info.i2c_base, info.i2c_clock_hz);
                let mut dev = Mpu6050::new(i2c, info.slave_addr);
                match dev.probe() {
                    Ok(true) => Ok(Sensor::Mpu6050Bsc(dev)),
                    _ => Err(SensorError::NoDevice),
                }
            }
            SensorSource::Mpu9250 => {
                let i2c = BscI2c::new(info.i2c_base, info.i2c_clock_hz);
                let mut dev = Mpu9250::new(i2c, info.slave_addr, info.mag_slave_addr);
                match dev.probe() {
                    Ok(true) => Ok(Sensor::Mpu9250Bsc(dev)),
                    _ => Err(SensorError::NoDevice),
                }
            }
        }
    }
}

impl SensorDevice for Sensor {
    fn whoami(&mut self) -> Result<u8, SensorError> {
        match self {
            Sensor::Sim(d) => d.whoami(),
            Sensor::Mpu6050Bsc(d) => d.whoami(),
            Sensor::Mpu9250Bsc(d) => d.whoami(),
            Sensor::Mpu6050Sim(d) => d.whoami(),
        }
    }
    fn probe(&mut self) -> Result<bool, SensorError> {
        match self {
            Sensor::Sim(d) => d.probe(),
            Sensor::Mpu6050Bsc(d) => d.probe(),
            Sensor::Mpu9250Bsc(d) => d.probe(),
            Sensor::Mpu6050Sim(d) => d.probe(),
        }
    }
    fn reset(&mut self) -> Result<(), SensorError> {
        match self {
            Sensor::Sim(d) => d.reset(),
            Sensor::Mpu6050Bsc(d) => d.reset(),
            Sensor::Mpu9250Bsc(d) => d.reset(),
            Sensor::Mpu6050Sim(d) => d.reset(),
        }
    }
    fn configure(&mut self, cfg: &SensorConfig) -> Result<(), SensorError> {
        match self {
            Sensor::Sim(d) => d.configure(cfg),
            Sensor::Mpu6050Bsc(d) => d.configure(cfg),
            Sensor::Mpu9250Bsc(d) => d.configure(cfg),
            Sensor::Mpu6050Sim(d) => d.configure(cfg),
        }
    }
    fn read_sample(&mut self) -> Result<SensorSample, SensorError> {
        match self {
            Sensor::Sim(d) => d.read_sample(),
            Sensor::Mpu6050Bsc(d) => d.read_sample(),
            Sensor::Mpu9250Bsc(d) => d.read_sample(),
            Sensor::Mpu6050Sim(d) => d.read_sample(),
        }
    }
    fn accel_lsb_per_g(&self) -> f32 {
        match self {
            Sensor::Sim(d) => d.accel_lsb_per_g(),
            Sensor::Mpu6050Bsc(d) => d.accel_lsb_per_g(),
            Sensor::Mpu9250Bsc(d) => d.accel_lsb_per_g(),
            Sensor::Mpu6050Sim(d) => d.accel_lsb_per_g(),
        }
    }
    fn gyro_lsb_per_dps(&self) -> f32 {
        match self {
            Sensor::Sim(d) => d.gyro_lsb_per_dps(),
            Sensor::Mpu6050Bsc(d) => d.gyro_lsb_per_dps(),
            Sensor::Mpu9250Bsc(d) => d.gyro_lsb_per_dps(),
            Sensor::Mpu6050Sim(d) => d.gyro_lsb_per_dps(),
        }
    }
    fn have_mag(&self) -> bool {
        match self {
            Sensor::Sim(d) => d.have_mag(),
            Sensor::Mpu6050Bsc(d) => d.have_mag(),
            Sensor::Mpu9250Bsc(d) => d.have_mag(),
            Sensor::Mpu6050Sim(d) => d.have_mag(),
        }
    }
    fn sample_rate(&self) -> u32 {
        match self {
            Sensor::Sim(d) => d.sample_rate(),
            Sensor::Mpu6050Bsc(d) => d.sample_rate(),
            Sensor::Mpu9250Bsc(d) => d.sample_rate(),
            Sensor::Mpu6050Sim(d) => d.sample_rate(),
        }
    }
    fn name(&self) -> &str {
        match self {
            Sensor::Sim(d) => d.name(),
            Sensor::Mpu6050Bsc(d) => d.name(),
            Sensor::Mpu9250Bsc(d) => d.name(),
            Sensor::Mpu6050Sim(d) => d.name(),
        }
    }
    fn set_batch(&mut self, enabled: bool, max: u8) -> Result<(), SensorError> {
        match self {
            Sensor::Sim(d) => d.set_batch(enabled, max),
            Sensor::Mpu6050Bsc(d) => d.set_batch(enabled, max),
            Sensor::Mpu9250Bsc(d) => d.set_batch(enabled, max),
            Sensor::Mpu6050Sim(d) => d.set_batch(enabled, max),
        }
    }
    fn fifo_count(&mut self) -> Result<u16, SensorError> {
        match self {
            Sensor::Sim(d) => d.fifo_count(),
            Sensor::Mpu6050Bsc(d) => d.fifo_count(),
            Sensor::Mpu9250Bsc(d) => d.fifo_count(),
            Sensor::Mpu6050Sim(d) => d.fifo_count(),
        }
    }
    fn enable_wake_on_motion(&mut self, t: u8, d: u8) -> Result<(), SensorError> {
        match self {
            Sensor::Sim(s) => s.enable_wake_on_motion(t, d),
            Sensor::Mpu6050Bsc(s) => s.enable_wake_on_motion(t, d),
            Sensor::Mpu9250Bsc(s) => s.enable_wake_on_motion(t, d),
            Sensor::Mpu6050Sim(s) => s.enable_wake_on_motion(t, d),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fusion::{euler_to_quat, gravity_in_body};

    #[test]
    fn test_mpu6050_over_sim_bus() {
        let mut bus = SimI2c::new();
        bus.set_accel_raw(16384, 0, 0);
        bus.set_gyro_raw(0, 131, 0);
        let mut dev = Mpu6050::new(bus, 0x68);
        assert!(dev.probe().unwrap());
        assert_eq!(dev.whoami().unwrap(), 0x68);
        dev.configure(&SensorConfig::default()).unwrap();
        let s = dev.read_sample().unwrap();
        assert_eq!(s.accel.x, 16384);
        assert_eq!(s.gyro.y, 131);
        let (a, g, _) = s.to_physical(dev.accel_lsb_per_g(), dev.gyro_lsb_per_dps());
        assert!((a.x - 1.0).abs() < 1e-3, "accel.x {}", a.x);
        assert!((g.y - 1.0).abs() < 1e-3, "gyro.y {}", g.y);
    }

    #[test]
    fn test_mpu9250_over_sim_bus() {
        let mut bus = SimI2c::new();
        bus.set_accel_raw(0, 0, 16384);
        bus.set_gyro_raw(0, 0, 0);
        // AK8963 寄存器注入（WIA + 调整）。
        bus.write_reg(0x0C, 0x00, 0x48).unwrap();
        bus.write_reg(0x0C, 0x10, 128).unwrap(); // adj = 0（中性）
        bus.write_reg(0x0C, 0x11, 128).unwrap();
        bus.write_reg(0x0C, 0x12, 128).unwrap();
        let mut dev = Mpu9250::new(bus, 0x68, 0x0C);
        assert!(dev.probe().unwrap());
        dev.configure(&SensorConfig::default()).unwrap();
        // 磁力计数据注入（ST1 DRDY + HXL）。
        dev.imu.i2c.write_reg(0x0C, 0x02, 0x01).unwrap();
        dev.imu.i2c.write_reg(0x0C, 0x03, 0x00).unwrap();
        dev.imu.i2c.write_reg(0x0C, 0x04, 0x19).unwrap(); // x = 6400
        dev.imu.i2c.write_reg(0x0C, 0x05, 0x00).unwrap();
        dev.imu.i2c.write_reg(0x0C, 0x06, 0x00).unwrap();
        dev.imu.i2c.write_reg(0x0C, 0x07, 0x00).unwrap();
        dev.imu.i2c.write_reg(0x0C, 0x08, 0x00).unwrap();
        let s = dev.read_sample().unwrap();
        assert_eq!(s.mag_valid, 1);
        assert_eq!(s.mag.x, 6400);
    }

    #[test]
    fn test_sample_abi_roundtrip() {
        let s = SensorSample {
            seq: 7,
            ts_us: 123456789,
            accel: Vec3i16::new(1, -2, 3),
            gyro: Vec3i16::new(-4, 5, -6),
            mag: Vec3i16::new(7, -8, 9),
            mag_valid: 1,
            reserved: [0; 3],
        };
        let b = s.to_bytes();
        assert_eq!(b.len(), SensorSample::SIZE);
        let s2 = SensorSample::from_bytes(&b).unwrap();
        assert_eq!(s2.seq, s.seq);
        assert_eq!(s2.ts_us, s.ts_us);
        assert_eq!(s2.accel, s.accel);
        assert_eq!(s2.gyro, s.gyro);
        assert_eq!(s2.mag, s.mag);
        assert_eq!(s2.mag_valid, 1);
    }

    #[test]
    fn test_sim_sensor_script_flat() {
        let mut s = SimSensor::new();
        s.set_mode(SimMode::Fixed { roll: 0.0, pitch: 0.0, yaw: 0.0 });
        s.configure(&SensorConfig::default()).unwrap();
        let sm = s.read_sample().unwrap();
        let (a, g, _) = sm.to_physical(s.accel_lsb_per_g(), s.gyro_lsb_per_dps());
        // 平放：加速度 ≈ (0,0,1)g，陀螺 ≈ 0。
        assert!((a.z - 1.0).abs() < 0.02, "a.z {}", a.z);
        assert!(g.norm() < 0.1, "gyro {}", g.norm());
    }

    #[test]
    fn test_sim_sensor_matches_gravity() {
        let mut s = SimSensor::new();
        s.set_mode(SimMode::Fixed { roll: 90.0, pitch: 0.0, yaw: 0.0 });
        s.configure(&SensorConfig::default()).unwrap();
        let sm = s.read_sample().unwrap();
        let (a, _, _) = sm.to_physical(s.accel_lsb_per_g(), s.gyro_lsb_per_dps());
        // 理论值：gravity_in_body(euler_to_quat(90,0,0)) = (0,1,0)。
        let expect = gravity_in_body(euler_to_quat(90.0, 0.0, 0.0));
        assert!((a.x - expect.x).abs() < 0.02);
        assert!((a.y - expect.y).abs() < 0.02);
        assert!((a.z - expect.z).abs() < 0.02);
    }
}

// ==================== /dev/sensor0 内核 ABI ====================
//
// 内核把传感器封装为字符设备 /dev/sensor0，用户态 sensord 读取原始样本
// （[`SensorSample`]，40 字节）与设备信息（[`SensorInfoC`]）。

/// ioctl：读取设备信息（[`SensorInfoC`]，28 字节）。
pub const SENSOR_IOCTL_GET_INFO: u32 = 0x5200;
/// ioctl：设置采样率（预留：当前内核固定 100Hz，未来扩展输入参数路径）。
pub const SENSOR_IOCTL_SET_RATE: u32 = 0x5201;

/// /dev/sensor0 ioctl 信息结构（ABI，28 字节小端）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SensorInfoC {
    /// 0=模拟 / 1=MPU6050 / 2=MPU9250。
    pub source: u8,
    /// 是否有磁力计（0/1）。
    pub have_mag: u8,
    /// 加速度量程枚举（0..3 = ±2/4/8/16g）。
    pub accel_range: u8,
    /// 陀螺量程枚举（0..3 = ±250/500/1000/2000dps）。
    pub gyro_range: u8,
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 加速度灵敏度（LSB/g）。
    pub accel_lsb_per_g: f32,
    /// 陀螺灵敏度（LSB/(°/s)）。
    pub gyro_lsb_per_dps: f32,
    /// 保留。
    pub reserved: [u8; 12],
}

impl SensorInfoC {
    /// ABI 字节长度。
    pub const SIZE: usize = 28;

    /// 序列化为小端字节。
    pub fn to_bytes(&self) -> [u8; SensorInfoC::SIZE] {
        let mut b = [0u8; SensorInfoC::SIZE];
        b[0] = self.source;
        b[1] = self.have_mag;
        b[2] = self.accel_range;
        b[3] = self.gyro_range;
        b[4..8].copy_from_slice(&self.sample_rate.to_le_bytes());
        b[8..12].copy_from_slice(&self.accel_lsb_per_g.to_le_bytes());
        b[12..16].copy_from_slice(&self.gyro_lsb_per_dps.to_le_bytes());
        b
    }

    /// 从小端字节解析。
    pub fn from_bytes(b: &[u8]) -> Option<SensorInfoC> {
        if b.len() < SensorInfoC::SIZE {
            return None;
        }
        Some(SensorInfoC {
            source: b[0],
            have_mag: b[1],
            accel_range: b[2],
            gyro_range: b[3],
            sample_rate: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            accel_lsb_per_g: f32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            gyro_lsb_per_dps: f32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            reserved: [0; 12],
        })
    }
}
