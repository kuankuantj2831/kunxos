//! 传感器子系统：/dev/sensor0 字符设备。
//!
//! 职责：
//! - 按平台信息（[`kunxhal::sensor::SensorInfo`]）打开传感器后端：
//!   QEMU 为软件模拟（确定性剧本），Pi4 为 BSC1 I2C + MPU6050/9250
//!   （真实探测失败回退模拟器）。
//! - read：返回一个 [`kunxhal::sensor::SensorSample`]（40 字节，小端），
//!   每次读取推进一帧（模拟器按采样率推进仿真时钟；真实器件做一次 I2C 突发读）。
//! - ioctl：`SENSOR_IOCTL_GET_INFO` 返回 [`SensorInfoC`]（量程/灵敏度/采样率）。
//!   采样率当前固定 100Hz（`SENSOR_IOCTL_SET_RATE` 预留）。
//!
//! 低功耗：Pi4 无传感器 Hub，主 CPU 直接处理；MPU 运动唤醒（INT 引脚 →
//! GPIO4 → GPIO IRQ，GIC SPI 96+32=128）为预留路径，见 `docs/sensors.md`。

use kunxhal::sensor::{
    Sensor, SensorConfig, SensorDevice, SensorInfo, SensorInfoC, SensorSample, SensorSource,
    SENSOR_IOCTL_GET_INFO,
};

use crate::devfs;
use crate::timer;
use crate::uart_send_str;
use crate::vfs;

/// /dev/sensor0 主设备号。
pub const SENSOR_MAJOR: u32 = 32;
/// /dev/sensor0 次设备号。
pub const SENSOR_MINOR: u32 = 0;

/// 内核侧传感器状态。
static mut DEV: Option<Sensor> = None;
/// 设备信息缓存（ioctl GET_INFO 用）。
static mut INFO: SensorInfoC = SensorInfoC {
    source: 0,
    have_mag: 0,
    accel_range: 0,
    gyro_range: 0,
    sample_rate: 100,
    accel_lsb_per_g: 16384.0,
    gyro_lsb_per_dps: 131.0,
    reserved: [0; 12],
};
/// 样本序号。
static mut SEQ: u32 = 0;

/// `/dev/sensor0` 的 read：返回一个完整样本（40 字节）。
fn sensor_read(_minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    unsafe {
        if buf.len() < SensorSample::SIZE {
            return 0;
        }
        let dev = match &mut DEV {
            Some(d) => d,
            None => return -19, // ENODEV
        };
        match dev.read_sample() {
            Ok(mut sm) => {
                SEQ = SEQ.wrapping_add(1);
                sm.seq = SEQ;
                sm.ts_us = timer::now_us();
                let bytes = sm.to_bytes();
                buf[..SensorSample::SIZE].copy_from_slice(&bytes);
                SensorSample::SIZE as i32
            }
            Err(_) => -5, // EIO
        }
    }
}

/// `/dev/sensor0` 的 write：不支持。
fn sensor_write(_minor: u32, _buf: &[u8]) -> i32 {
    -1
}

/// `/dev/sensor0` 的 ioctl：GET_INFO 返回设备信息（28 字节）。
fn sensor_ioctl(_minor: u32, request: u32, out: &mut [u8]) -> i32 {
    match request {
        SENSOR_IOCTL_GET_INFO => {
            let bytes = unsafe { INFO.to_bytes() };
            let n = bytes.len().min(out.len());
            out[..n].copy_from_slice(&bytes[..n]);
            n as i32
        }
        _ => -25, // ENOTTY
    }
}

/// 初始化传感器子系统：打开后端、注册设备节点。
///
/// `info == None` 表示平台无传感器总线，不注册设备（sensord 打开失败即
/// 报告「无传感器」）。
pub fn init(info: Option<SensorInfo>) {
    let Some(info) = info else {
        crate::kinfo! { uart_send_str("[sensor] 此平台无传感器设备\n"); }
        return;
    };

    // 真实器件（Pi4）：先启用 I2C1 引脚（GPIO 2/3 → ALT0 + 上拉）。
    if info.source != SensorSource::Sim {
        kunxhal::sensor::bsc1_gpio_init();
        crate::kinfo! { uart_send_str("[sensor] bsc1 gpio (SDA=GPIO2, SCL=GPIO3) 已启用\n"); }
    }

    // 打开传感器；真实器件探测失败回退模拟器，保证系统可运行。
    let mut dev = match Sensor::open(&info) {
        Ok(d) => d,
        Err(_) => {
            crate::kinfo! { uart_send_str("[sensor] WARNING: 未找到真实 IMU，改用模拟器\n"); }
            Sensor::Sim(kunxhal::sensor::SimSensor::new())
        }
    };

    unsafe {
        // 配置（默认 100Hz，±2g / ±250dps）。
        let cfg = SensorConfig::default();
        let _ = dev.configure(&cfg);
        INFO.source = match info.source {
            SensorSource::Sim => 0,
            SensorSource::Mpu6050 => 1,
            SensorSource::Mpu9250 => 2,
        };
        INFO.have_mag = dev.have_mag() as u8;
        INFO.accel_range = 0; // G2
        INFO.gyro_range = 0; // Dps250
        INFO.sample_rate = dev.sample_rate();
        INFO.accel_lsb_per_g = dev.accel_lsb_per_g();
        INFO.gyro_lsb_per_dps = dev.gyro_lsb_per_dps();
        DEV = Some(dev);
    }

    devfs::register_chrdev(
        SENSOR_MAJOR,
        "sensor",
        devfs::ChrOps { read: sensor_read, write: sensor_write, ioctl: sensor_ioctl },
    );
    vfs::mknod("/dev/sensor0", vfs::FileType::Chr, SENSOR_MAJOR, SENSOR_MINOR);

    crate::kinfo! {
        uart_send_str("[sensor] /dev/sensor0 已注册 (");
        uart_send_str(match info.source {
            SensorSource::Sim => "sim-imu",
            SensorSource::Mpu6050 => "mpu6050",
            SensorSource::Mpu9250 => "mpu9250",
        });
        uart_send_str(", 100Hz, mag=");
        uart_send_str(if unsafe { INFO.have_mag } != 0 { "yes" } else { "no" });
        uart_send_str(")\n");
    }
}
