//! 相机硬件抽象层（camera HAL）：平台无关的相机设备接口。
//!
//! 分层（与 `docs/camera.md` 架构图对应）：
//! ```text
//! 应用 (apps/camera) → 相机服务 (kunxserv/camerad) → kunxhal::camera →
//!     平台后端：QEMU 软件传感器（SimCamera）/ Pi4 固件+CSI（PiCamera）
//! ```
//!
//! 设计要点：
//! - [`CameraDevice`] trait 是唯一设备抽象：枚举 / 打开 / 配置 / 起停采集 /
//!   单帧拍照 / 3A 参数，QEMU 与 Pi4 后端分别实现。
//! - [`CaptureRequest`] / [`CaptureResult`] 描述一次采集请求与结果（含时间戳、
//!   曝光/增益/白平衡元数据与缓冲区索引）。
//! - [`BufferPool`] 管理 CPU/DMA 双用缓冲：后端把帧写入池中槽位，消费者按
//!   索引取用；无堆分配，缓冲存储由后端静态提供。
//! - 3A（AE/AF/AWB）为 trait 化框架，[`FixedThreeA`] 为固定参数实现
//!   （曝光/增益/白平衡恒定），后续可替换为统计反馈算法。
//!
//! `no_std`，无堆分配，无平台指令（后端实现才接触寄存器/汇编）。

use core::fmt;

#[cfg(test)]
extern crate std;

// ==================== 枚举与结构 ====================

/// 像素格式。ISP 未实现前以 RAW/YUV 直出为主。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 传感器 RAW（Bayer 8bit，未做去马赛克）。
    RawBayer8,
    /// YUV 4:2:0 planar（I420：Y 平面 + 交错 U/V 平面）。
    Yuv420,
    /// RGB888 行主序（软件转换后的通用格式）。
    Rgb888,
}

impl PixelFormat {
    /// 名称（用于日志与协议文本）。
    pub fn name(&self) -> &'static str {
        match self {
            PixelFormat::RawBayer8 => "raw8",
            PixelFormat::Yuv420 => "yuv420",
            PixelFormat::Rgb888 => "rgb888",
        }
    }
}

/// 摄像头朝向（多摄像头预留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraFacing {
    /// 后置（主摄）。
    Back,
    /// 前置（自拍）。
    Front,
}

impl CameraFacing {
    pub fn name(&self) -> &'static str {
        match self {
            CameraFacing::Back => "back",
            CameraFacing::Front => "front",
        }
    }
}

/// 相机采集配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraConfig {
    pub width: usize,
    pub height: usize,
    /// 帧率（fps）。
    pub fps: usize,
    pub format: PixelFormat,
}

impl CameraConfig {
    pub const fn new(width: usize, height: usize, fps: usize, format: PixelFormat) -> Self {
        CameraConfig {
            width,
            height,
            fps,
            format,
        }
    }

    /// 单帧字节数（Yuv420 为 w*h*1.5，其余按位深）。
    pub fn frame_size(&self) -> usize {
        match self.format {
            PixelFormat::RawBayer8 => self.width * self.height,
            PixelFormat::Yuv420 => self.width * self.height * 3 / 2,
            PixelFormat::Rgb888 => self.width * self.height * 3,
        }
    }

    /// 一秒钟的数据量（带宽估算）。
    pub fn bytes_per_second(&self) -> usize {
        self.frame_size() * self.fps
    }
}

/// 相机设备信息（枚举结果）。
#[derive(Debug, Clone, Copy)]
pub struct CameraDeviceInfo {
    /// 设备 ID（camerad 协议中的相机序号）。
    pub id: u32,
    pub name: &'static str,
    pub facing: CameraFacing,
    /// 支持的最大分辨率。
    pub max_width: usize,
    pub max_height: usize,
    /// 支持的格式位图（bit0=raw8, bit1=yuv420, bit2=rgb888）。
    pub formats: u8,
}

/// 采集请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRequest {
    /// 目标相机 ID。
    pub camera_id: u32,
    pub config: CameraConfig,
    /// 单帧拍照（true）或连续采集（false，供预览/录像）。
    pub single: bool,
    /// 期望使用的缓冲槽位掩码（0 = 由设备决定）。
    pub buffer_mask: u32,
}

/// 采集结果（一帧）。
#[derive(Debug, Clone, Copy)]
pub struct CaptureResult {
    /// 帧序号（连续采集时单调递增）。
    pub seq: u32,
    /// 采集时间戳（微秒，单调时钟）。
    pub ts_us: u64,
    pub width: usize,
    pub height: usize,
    pub format: PixelFormat,
    /// 缓冲区槽位索引（数据在对应 BufferPool 槽位中）。
    pub buffer_index: usize,
    /// 3A 元数据。
    pub exposure_us: u32,
    pub iso_gain: u16,
    pub awb_temp_k: u16,
    /// 对焦马达位置（0..100，固定算法下恒为默认值）。
    pub focus_pos: u8,
}

/// 错误类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraError {
    /// 无此相机 / 设备未接。
    NotFound,
    /// 相机被占用（未关闭即重新打开）。
    Busy,
    /// 配置不被支持。
    Unsupported,
    /// 缓冲池满。
    NoBuffer,
    /// 后端硬件未实现（Pi4 骨架返回此错误）。
    NotImplemented,
    /// 参数非法。
    InvalidArg,
}

impl fmt::Display for CameraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CameraError::NotFound => "not found",
            CameraError::Busy => "busy",
            CameraError::Unsupported => "unsupported",
            CameraError::NoBuffer => "no buffer",
            CameraError::NotImplemented => "not implemented",
            CameraError::InvalidArg => "invalid argument",
        };
        f.write_str(s)
    }
}

// ==================== 缓冲管理 ====================

/// 缓冲槽状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferState {
    /// 空闲，可被写入。
    Free,
    /// 已填充一帧，等待消费。
    Filled,
}

/// 相机帧缓冲池：`N` 个槽位，后端写入、消费者读取。
///
/// 后端负责把帧渲染/搬运到槽位；消费者先 [`BufferPool::acquire`] 取走已填充
/// 槽位（读取其数据），处理完 [`BufferPool::release`] 归还。无堆分配。
pub struct BufferPool<const N: usize> {
    states: [BufferState; N],
    /// 每个槽位对应的物理地址（DMA 场景由后端填入；CPU 场景为 0）。
    phys: [usize; N],
}

impl<const N: usize> BufferPool<N> {
    pub const fn new() -> Self {
        BufferPool {
            states: [BufferState::Free; N],
            phys: [0; N],
        }
    }

    /// 取一个空闲槽位供写入；无空闲返回 None。
    pub fn acquire_free(&mut self) -> Option<usize> {
        for i in 0..N {
            if self.states[i] == BufferState::Free {
                self.states[i] = BufferState::Filled;
                return Some(i);
            }
        }
        None
    }

    /// 取走一个已填充槽位（消费者读完须调用 [`BufferPool::release`]）。
    pub fn acquire_filled(&mut self) -> Option<usize> {
        for i in 0..N {
            if self.states[i] == BufferState::Filled {
                return Some(i);
            }
        }
        None
    }

    /// 归还槽位（消费者处理完毕 / 后端重写）。
    pub fn release(&mut self, index: usize) {
        if index < N {
            self.states[index] = BufferState::Free;
        }
    }

    pub fn state(&self, index: usize) -> BufferState {
        if index < N {
            self.states[index]
        } else {
            BufferState::Free
        }
    }

    /// 设置槽位的物理地址（DMA 后端用）。
    pub fn set_phys(&mut self, index: usize, phys: usize) {
        if index < N {
            self.phys[index] = phys;
        }
    }

    pub fn phys(&self, index: usize) -> usize {
        if index < N {
            self.phys[index]
        } else {
            0
        }
    }
}

// ==================== 3A 算法框架 ====================

/// 自动曝光参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AeParams {
    /// 曝光时间（微秒）。
    pub exposure_us: u32,
    /// 增益（ISO 等效）。
    pub iso: u16,
}

/// 自动对焦参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AfParams {
    /// 对焦马达位置 0..100。
    pub position: u8,
}

/// 自动白平衡参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwbParams {
    /// 色温（开尔文）。
    pub temp_k: u16,
    /// R 增益（16.16 定点）。
    pub gain_r: u32,
    /// B 增益（16.16 定点）。
    pub gain_b: u32,
}

/// 一次 3A 状态快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreeAParams {
    pub ae: AeParams,
    pub af: AfParams,
    pub awb: AwbParams,
}

impl ThreeAParams {
    /// 常用默认：1/30s、ISO 100、5500K 日光。
    pub const fn defaults() -> Self {
        ThreeAParams {
            ae: AeParams {
                exposure_us: 33_333,
                iso: 100,
            },
            af: AfParams { position: 50 },
            awb: AwbParams {
                temp_k: 5500,
                gain_r: 1 << 16,
                gain_b: 1 << 16,
            },
        }
    }
}

/// 帧统计（3A 算法输入；固定算法可忽略）。
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStats {
    /// 平均亮度（0..255）。
    pub mean_luma: u8,
    /// 中心区域对焦评价（越大越清晰；固定 AF 忽略）。
    pub focus_metric: u16,
    /// R/G/B 通道均值（AWB 输入）。
    pub mean_r: u8,
    pub mean_g: u8,
    pub mean_b: u8,
}

/// 3A 算法 trait：输入帧统计，输出新的曝光/对焦/白平衡参数。
///
/// [`FixedThreeA`] 为固定参数实现（当前默认）；后续可在 camerad 中实现
/// 基于统计的简化 AE（如目标亮度 128 的 PID）与 AWB（灰世界校正）。
pub trait ThreeA {
    /// 依据上一帧统计更新参数。
    fn update(&mut self, stats: &FrameStats) -> ThreeAParams;
    /// 当前参数快照。
    fn params(&self) -> ThreeAParams;
    /// 手动设定（覆盖自动结果）。
    fn set(&mut self, params: ThreeAParams);
}

/// 固定参数 3A：不随帧变化，始终返回初始值。
#[derive(Debug, Clone, Copy)]
pub struct FixedThreeA {
    params: ThreeAParams,
}

impl FixedThreeA {
    pub const fn new(params: ThreeAParams) -> Self {
        FixedThreeA { params }
    }
}

impl ThreeA for FixedThreeA {
    fn update(&mut self, _stats: &FrameStats) -> ThreeAParams {
        self.params
    }
    fn params(&self) -> ThreeAParams {
        self.params
    }
    fn set(&mut self, params: ThreeAParams) {
        self.params = params;
    }
}

// ==================== 设备 trait ====================

/// 相机设备抽象。平台后端实现本 trait：
/// - QEMU：`SimCamera`（软件传感器，生成彩条/计数器测试帧）
/// - Pi4：`PiCamera`（骨架：mailbox + Unicam CSI-2 寄存器编程，待硬件接线）
pub trait CameraDevice {
    /// 枚举可用相机（返回信息数组，最多 `out.len()` 个）。
    fn enumerate(&self, out: &mut [CameraDeviceInfo]) -> usize;

    /// 打开相机（占用设备；重复打开返回 Busy）。
    fn open(&mut self, id: u32) -> Result<(), CameraError>;

    /// 关闭相机。
    fn close(&mut self) -> Result<(), CameraError>;

    /// 应用采集配置（分辨率/帧率/格式）。
    fn configure(&mut self, config: &CameraConfig) -> Result<(), CameraError>;

    /// 开始连续采集（预览/录像）。帧经 [`CameraDevice::poll_frame`] 取回。
    fn start_stream(&mut self) -> Result<(), CameraError>;

    /// 停止连续采集。
    fn stop_stream(&mut self) -> Result<(), CameraError>;

    /// 取一帧已就绪的连续采集帧（非阻塞；无帧就绪返回 None）。
    ///
    /// 后端按配置帧率生成帧并写入缓冲池槽位；EL0 无中断/线程，调用方
    /// （camerad 主循环）以事件循环方式轮询本方法。
    fn poll_frame(&mut self) -> Option<CaptureResult>;

    /// 单帧拍照（阻塞到帧就绪，返回结果）。
    fn capture_single(&mut self, req: &CaptureRequest) -> Result<CaptureResult, CameraError>;

    /// 读取/设置 3A 参数。
    fn get_3a(&self) -> ThreeAParams;
    fn set_3a(&mut self, params: ThreeAParams);

    /// 帧数据访问：`result.buffer_index` 对应缓冲池槽位，读入 `dst`。
    fn read_frame(&mut self, result: &CaptureResult, dst: &mut [u8]) -> Result<usize, CameraError>;
}

// ==================== 便捷构造 ====================

/// 当前平台的相机后端实例（由 Cargo feature 选定）。
///
/// QEMU 返回软件传感器（SimCamera）；Pi4 返回硬件骨架（PiCamera）。
/// 单线程 EL0 服务（camerad）独占使用，无并发别名风险。
pub fn camera_backend() -> Option<&'static mut dyn CameraDevice> {
    #[cfg(feature = "pi4")]
    {
        return Some(unsafe { &mut *core::ptr::addr_of_mut!(crate::plat_pi4::PI_CAMERA) });
    }
    #[cfg(all(feature = "qemu", not(feature = "pi4")))]
    {
        return Some(unsafe { &mut *core::ptr::addr_of_mut!(crate::plat_qemu::SIM_CAMERA) });
    }
    #[cfg(not(any(feature = "pi4", feature = "qemu")))]
    {
        None
    }
}

// ==================== 宿主测试（CI 可跑，无需 QEMU） ====================

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "qemu")]
    use crate::plat_qemu::render_test_frame;
    use std::vec;
    use std::vec::Vec;

    /// BufferPool 槽位状态机。
    #[test]
    fn buffer_pool_acquire_release() {
        let mut pool = BufferPool::<4>::new();
        assert!(pool.acquire_filled().is_none(), "空池不应有已填充槽");
        let b = pool.acquire_free().expect("应有空闲槽");
        assert!(pool.acquire_filled() == Some(b), "写入后应可消费");
        pool.release(b);
        assert!(pool.acquire_filled().is_none(), "释放后不应有已填充槽");
        assert!(pool.acquire_free().is_some(), "释放后应可再获取");
    }

    /// BufferPool 物理地址存取（Pi4 DMA 接线语义）。
    #[test]
    fn buffer_pool_phys() {
        let mut pool = BufferPool::<3>::new();
        pool.set_phys(1, 0xFE80_1000);
        assert_eq!(pool.phys(1), 0xFE80_1000);
        assert_eq!(pool.phys(9), 0, "越界索引返回 0");
    }

    /// 帧尺寸与带宽计算。
    #[test]
    fn config_frame_size() {
        let y = CameraConfig::new(640, 480, 30, PixelFormat::Yuv420);
        assert_eq!(y.frame_size(), 640 * 480 * 3 / 2);
        assert_eq!(y.bytes_per_second(), y.frame_size() * 30);
        let rgb = CameraConfig::new(640, 480, 30, PixelFormat::Rgb888);
        assert_eq!(rgb.frame_size(), 640 * 480 * 3);
        let raw = CameraConfig::new(640, 480, 30, PixelFormat::RawBayer8);
        assert_eq!(raw.frame_size(), 640 * 480);
    }

    /// FixedThreeA 默认参数与手动覆盖。
    #[test]
    fn fixed_threea() {
        let mut t = FixedThreeA::new(ThreeAParams::defaults());
        let p = t.params();
        assert_eq!(p.ae.exposure_us, 33_333);
        assert_eq!(p.ae.iso, 100);
        assert_eq!(p.awb.temp_k, 5500);
        assert_eq!(p.af.position, 50);
        let s = FrameStats { mean_luma: 30, ..Default::default() };
        let p2 = t.update(&s);
        assert_eq!(p2, p, "固定算法不随统计变化");
        let mut custom = p;
        custom.ae.exposure_us = 10_000;
        t.set(custom);
        assert_eq!(t.params().ae.exposure_us, 10_000);
    }

    /// SimCamera 单帧 RGB888 拍照：读回数据尺寸与彩条内容一致。
    #[cfg(feature = "qemu")]
    #[test]
    fn sim_camera_single_frame() {
        let cam = camera_backend().expect("qemu 后端存在");
        cam.open(0).unwrap();
        let cfg = CameraConfig::new(160, 120, 1, PixelFormat::Rgb888);
        cam.configure(&cfg).unwrap();
        let req = CaptureRequest {
            camera_id: 0,
            config: cfg,
            single: true,
            buffer_mask: 0,
        };
        let r = cam.capture_single(&req).expect("capture");
        assert_eq!((r.width, r.height), (160, 120));
        let mut buf = vec![0u8; cfg.frame_size()];
        let size = cam.read_frame(&r, &mut buf).expect("read");
        assert_eq!(size, 160 * 120 * 3);
        // 计数条下方采样（y=10, x=5）：属 band0 纯红（避开计数条/运动块）
        let off = (10 * 160 + 5) * 3;
        assert_eq!(buf[off], 255, "band0: R");
        assert_eq!(buf[off + 1], 0, "band0: G");
        assert_eq!(buf[off + 2], 0, "band0: B");
        // 左上角帧计数条（seq=1 → 第 0 位为 1 → 白色）
        assert_eq!(buf[0], 255);
        assert_eq!(buf[1], 255);
        assert_eq!(buf[2], 255);
    }

    /// 测试帧确定性：同 seq/ts 渲染结果一致，不同 seq 帧计数条变化。
    #[test]
    fn render_frame_deterministic() {
        let w = 64;
        let h = 48;
        let mk = |seq: u32, ts: u64| CaptureResult {
            seq,
            ts_us: ts,
            width: w,
            height: h,
            format: PixelFormat::Yuv420,
            buffer_index: 0,
            exposure_us: 33_333,
            iso_gain: 100,
            awb_temp_k: 5500,
            focus_pos: 50,
        };
        let mut a = vec![0u8; w * h * 3 / 2];
        let mut b = vec![0u8; w * h * 3 / 2];
        render_test_frame(&mut a, &mk(7, 1234));
        render_test_frame(&mut b, &mk(7, 1234));
        assert_eq!(a, b, "同参数渲染必须逐字节一致");
        let mut c = vec![0u8; w * h * 3 / 2];
        render_test_frame(&mut c, &mk(8, 1234));
        assert_ne!(a, c, "帧序号变化时计数条应变化");
        // 帧计数条：左上 2x2 像素块为 seq 第 0 位；seq 奇偶决定黑白
        assert_eq!(a[0], 235, "seq=7(奇) 计数条第 0 位 = 1 → 白");
        assert_eq!(c[0], 16, "seq=8(偶) 计数条第 0 位 = 0 → 黑");
    }

    /// 枚举：SimCamera 返回 1 台后置相机。
    #[cfg(feature = "qemu")]
    #[test]
    fn sim_camera_enumerate() {
        let cam = camera_backend().expect("qemu 后端存在");
        let mut info = [CameraDeviceInfo {
            id: 0,
            name: "",
            facing: CameraFacing::Back,
            max_width: 0,
            max_height: 0,
            formats: 0,
        }; 4];
        let n = cam.enumerate(&mut info);
        assert_eq!(n, 1);
        assert_eq!(info[0].id, 0);
        assert_eq!(info[0].name, "sim-cam0");
        assert_eq!(info[0].facing, CameraFacing::Back);
        assert_eq!(info[0].formats, 0b111);
    }
}

