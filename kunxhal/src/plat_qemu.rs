//! QEMU `virt` 平台后端：硬编码现有设备的地址 / 中断号 / 时钟。
//!
//! 电源相关能力：
//!   - 关机 / 重启：PSCI 0.2（QEMU virt 的 PSCI conduit 为 hvc，见内核
//!     shell 的 `poweroff`/`reboot` 实现），经 EL2 固件退出模拟器。
//!   - 温度：QEMU 不模拟 SoC 温度传感器（virt DTB 无 thermal-zones），
//!     返回 `None`；内核策略引擎回退到「软件热模型」（由 CPU 负载驱动）。
//!   - DVFS：QEMU 不模拟时钟树，提供**虚拟 DVFS 控制器**（频点表 + 频率
//!     追踪，切换无硬件效果）——让 governor / 热节流 / 采样策略在 QEMU
//!     下可端到端验证（切换决策与日志真实，寄存器写为空操作）。
//!   - 电池：无，返回 `None`；内核侧使用虚拟电池模型测试状态机与事件。

use core::sync::atomic::{AtomicU32, Ordering};

use crate::display::DisplayInfo;
use crate::input::{InputInfo, InputKind, InputTransport};
use crate::net::NetInfo;
use crate::plat::Platform;
use crate::power::{freq_point, Battery, DvfsController, FreqPoint, PowerControl, ThermalSensor};
use crate::sensor::{SensorInfo, SensorSource};
use crate::storage::StorageInfo;

/// QEMU virt 平台单例。
pub static QEMU: QemuPlatform = QemuPlatform;

/// QEMU `virt` 机器（Cortex-A72）平台参数。
pub struct QemuPlatform;

impl Platform for QemuPlatform {
    fn name(&self) -> &str {
        "QEMU virt"
    }

    fn uart_base(&self) -> usize {
        0x0900_0000 // PL011
    }

    fn uart_irq(&self) -> u32 {
        33
    }

    fn uart_clock(&self) -> u32 {
        24_000_000 // 24MHz
    }

    fn gicd_base(&self) -> usize {
        0x0800_0000 // GICv2 Distributor
    }

    fn gicc_base(&self) -> usize {
        0x0801_0000 // GICv2 CPU Interface
    }

    fn ram_base(&self) -> usize {
        0x4000_0000
    }

    fn ram_size(&self) -> usize {
        0x2000_0000 // 512MB
    }

    fn timer_freq(&self) -> u64 {
        62_500_000 // CNTFRQ 62.5MHz
    }
    fn rtc_base(&self) -> Option<usize> {
        // QEMU virt PL031：RTC 位于 0x09010000（MMIO，DTB /pl031）
        Some(0x0901_0000)
    }

    fn display_init(&self) -> Option<DisplayInfo> {
        // ramfb：帧缓冲置于 RAM 偏移 128MB 处（0x40000000 + 0x08000000）。
        Some(DisplayInfo::new(0x4800_0000, 540, 960, 32))
    }

    fn net_init(&self) -> Option<NetInfo> {
        Some(NetInfo::new(
            [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
            0x0a00_3000,
            36,
        ))
    }

    fn storage_init(&self) -> Option<StorageInfo> {
        // ramdisk：内存存储（SFS 文件系统），无 MMIO 控制器。
        None
    }

    fn input_init(&self) -> Option<InputInfo> {
        // QEMU virt 无真实触摸屏：用主 PL011 UART 上的协议帧模拟绝对坐标触摸。
        // 面板物理范围与 ramfb 逻辑分辨率一致（540x960），launcher 据此做
        // 物理坐标 → 屏幕像素坐标的映射（此处为 1:1，真实面板按 max_x/max_y 缩放）。
        Some(InputInfo::new(
            InputKind::Touch,
            InputTransport::UartProto,
            self.uart_base(),
            self.uart_irq(),
            540,
            960,
        ))
    }

    fn power_control(&self) -> Option<&'static dyn PowerControl> {
        Some(&QEMU_POWER)
    }

    fn sensor_init(&self) -> Option<SensorInfo> {
        // QEMU virt 无真实 IMU：返回软件模拟传感器（确定性剧本：
        // 平放 → 抬腕 → 航向扫描 → 行走计步，见 kunxhal::sensor::SimSensor）。
        Some(SensorInfo::new(
            SensorSource::Sim,
            0,
            0,
            0,
            0,
            0,
        ))
    }

    fn thermal_sensor(&self) -> Option<&'static dyn ThermalSensor> {
        None // QEMU 无温度传感器：内核侧用软件热模型
    }

    fn dvfs_controller(&self) -> Option<&'static dyn DvfsController> {
        Some(&QEMU_DVFS)
    }

    fn battery(&self) -> Option<&'static dyn Battery> {
        None // QEMU 无电池：内核侧用虚拟电池模型
    }

    fn wifi_init(&self) -> Option<&'static mut dyn crate::wifi::WifiDevice> {
        // QEMU 无真实无线硬件：返回软件模拟后端 SimWifi（真实 4-way 密码学链路，
        // 见文件尾部「Wi-Fi：软件模拟后端」一节）。
        Some(sim_wifi())
    }
}

// ==================== PSCI 电源控制（hvc conduit） ====================

/// PSCI 0.2 函数 ID（arm, psci-0.2 spec）。
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

/// QEMU PSCI 电源控制单例。
pub static QEMU_POWER: QemuPower = QemuPower;

/// QEMU 电源控制：`hvc #0` 交给 QEMU 固件（DTB psci 节点 method="hvc"）。
pub struct QemuPower;

impl PowerControl for QemuPower {
    fn shutdown(&self) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("hvc #0", in("x0") PSCI_SYSTEM_OFF, options(noreturn));
        }
        #[cfg(not(target_arch = "aarch64"))]
        loop {}
    }

    fn reboot(&self) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("hvc #0", in("x0") PSCI_SYSTEM_RESET, options(noreturn));
        }
        #[cfg(not(target_arch = "aarch64"))]
        loop {}
    }

    // sleep：QEMU virt 无低功耗挂起支持，保持运行。
}

// ==================== 虚拟 DVFS 控制器 ====================

/// 虚拟频点表：与 Pi4 的 Cortex-A72 频点一致（600/900/1200/1500 MHz），
/// 让 QEMU 下的 governor 决策与真机一致（切换本身无硬件效果）。
static VIRT_FREQ_TABLE: [FreqPoint; 4] = [
    freq_point(600_000_000, 0),
    freq_point(900_000_000, 0),
    freq_point(1_200_000_000, 0),
    freq_point(1_500_000_000, 0),
];

/// 当前虚拟频率（Hz，按簇；单簇平台只用 [0]）。
static VIRT_CUR_HZ: [AtomicU32; 4] = [
    AtomicU32::new(1_500_000_000),
    AtomicU32::new(1_500_000_000),
    AtomicU32::new(1_500_000_000),
    AtomicU32::new(1_500_000_000),
];

/// QEMU 虚拟 DVFS 控制器单例。
pub static QEMU_DVFS: QemuDvfs = QemuDvfs;

/// QEMU 虚拟 DVFS：频点表与频率追踪真实，寄存器写为空操作。
pub struct QemuDvfs;

impl DvfsController for QemuDvfs {
    fn cluster_count(&self) -> usize {
        1
    }

    fn freq_table(&self, cluster: usize) -> &'static [FreqPoint] {
        if cluster == 0 {
            &VIRT_FREQ_TABLE
        } else {
            &[]
        }
    }

    fn current_hz(&self, cluster: usize) -> u32 {
        if cluster < 4 {
            VIRT_CUR_HZ[cluster].load(Ordering::Relaxed)
        } else {
            0
        }
    }

    fn set_hz(&self, cluster: usize, hz: u32) -> Option<u32> {
        // 对齐到频点表中「不超过请求值」的最近频点（与 Pi4 固件 SET_CLOCK_RATE
        // 会 clamp 到支持范围的行为一致），并记录为当前频率。
        if cluster >= 4 {
            return None;
        }
        let mut applied = 0u32;
        for p in &VIRT_FREQ_TABLE {
            if p.hz <= hz {
                applied = p.hz;
            } else {
                break;
            }
        }
        if applied == 0 {
            applied = VIRT_FREQ_TABLE[0].hz;
        }
        VIRT_CUR_HZ[cluster].store(applied, Ordering::Relaxed);
        Some(applied)
    }
}

// ==================== 相机：软件模拟传感器（SimCamera） ====================
//
// QEMU virt 无真实相机（无 CSI / 无相机模块），SimCamera 是纯软件「传感器」：
// 按配置帧率生成确定性测试帧（彩条 + 运动白块 + 左上角二进制帧计数条 +
// 亮度渐变），供 camerad / 相机应用在 QEMU 下端到端验证采集、预览与拍照链路。
//
// 内存策略：不持有大帧缓冲——帧是 (seq, ts_us, config) 的确定性函数，
// read_frame 按需重渲染进调用方缓冲；BufferPool（HAL 的 DMA/CPU 双用池）
// 留给 Pi4 等硬件后端使用。

use crate::camera::{
    CameraConfig, CameraDevice, CameraDeviceInfo, CameraError, CameraFacing, CaptureRequest,
    CaptureResult, FixedThreeA, PixelFormat, ThreeA, ThreeAParams,
};

/// SimCamera 支持的最大分辨率（帧按需渲染，无需静态缓冲）。
pub const SIM_MAX_W: usize = 1280;
pub const SIM_MAX_H: usize = 720;

/// 无时钟时的占位（camerad 启动时注入真实时钟）。
fn sim_now_us_stub() -> u64 {
    0
}

/// QEMU 软件相机单例。
pub static mut SIM_CAMERA: SimCamera = SimCamera::new(sim_now_us_stub);

/// 注入单调时钟（camerad 启动时调用；SimCamera 的帧率/时间戳依赖真实时钟）。
pub fn sim_camera_set_clock(f: fn() -> u64) {
    unsafe {
        SIM_CAMERA.now_us = f;
    }
}

/// QEMU 软件传感器：确定性测试帧生成器。
pub struct SimCamera {
    /// 单调时钟（微秒），由使用者注入。
    pub now_us: fn() -> u64,
    open: bool,
    streaming: bool,
    config: CameraConfig,
    seq: u32,
    next_due_us: u64,
    threea: FixedThreeA,
}

impl SimCamera {
    pub const fn new(now_us: fn() -> u64) -> Self {
        SimCamera {
            now_us,
            open: false,
            streaming: false,
            config: CameraConfig::new(640, 480, 30, PixelFormat::Yuv420),
            seq: 0,
            next_due_us: 0,
            threea: FixedThreeA::new(ThreeAParams::defaults()),
        }
    }

    /// 是否到该出帧的时刻（按配置帧率）。
    fn frame_due(&mut self) -> bool {
        let now = (self.now_us)();
        if now >= self.next_due_us {
            self.next_due_us = now + 1_000_000 / self.config.fps.max(1) as u64;
            true
        } else {
            false
        }
    }

    /// 产出一帧（连续或单帧共用）：返回结果（buffer_index 恒 0，数据按需渲染）。
    fn produce_frame(&mut self) -> CaptureResult {
        self.seq = self.seq.wrapping_add(1);
        let p = self.threea.params();
        CaptureResult {
            seq: self.seq,
            ts_us: (self.now_us)(),
            width: self.config.width,
            height: self.config.height,
            format: self.config.format,
            buffer_index: 0,
            exposure_us: p.ae.exposure_us,
            iso_gain: p.ae.iso,
            awb_temp_k: p.awb.temp_k,
            focus_pos: p.af.position,
        }
    }
}

impl CameraDevice for SimCamera {
    fn enumerate(&self, out: &mut [CameraDeviceInfo]) -> usize {
        if out.is_empty() {
            return 0;
        }
        out[0] = CameraDeviceInfo {
            id: 0,
            name: "sim-cam0",
            facing: CameraFacing::Back,
            max_width: SIM_MAX_W,
            max_height: SIM_MAX_H,
            formats: 0b111, // raw8 | yuv420 | rgb888
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
        if config.width > SIM_MAX_W || config.height > SIM_MAX_H {
            return Err(CameraError::Unsupported);
        }
        self.config = *config;
        Ok(())
    }

    fn start_stream(&mut self) -> Result<(), CameraError> {
        if !self.open {
            return Err(CameraError::InvalidArg);
        }
        self.streaming = true;
        self.next_due_us = (self.now_us)();
        Ok(())
    }

    fn stop_stream(&mut self) -> Result<(), CameraError> {
        self.streaming = false;
        Ok(())
    }

    fn poll_frame(&mut self) -> Option<CaptureResult> {
        if !self.streaming || !self.frame_due() {
            return None;
        }
        Some(self.produce_frame())
    }

    fn capture_single(&mut self, _req: &CaptureRequest) -> Result<CaptureResult, CameraError> {
        if !self.open {
            return Err(CameraError::InvalidArg);
        }
        Ok(self.produce_frame())
    }

    fn get_3a(&self) -> ThreeAParams {
        self.threea.params()
    }

    fn set_3a(&mut self, params: ThreeAParams) {
        self.threea.set(params);
    }

    fn read_frame(
        &mut self,
        result: &CaptureResult,
        dst: &mut [u8],
    ) -> Result<usize, CameraError> {
        let size = self.config.frame_size();
        if dst.len() < size {
            return Err(CameraError::InvalidArg);
        }
        render_test_frame(&mut dst[..size], result);
        Ok(size)
    }
}

/// 渲染确定性测试帧（YUV420 或 RGB888 或 RAW8）。
///
/// 内容（YUV420）：
/// - 7 段彩条（SMPTE 风格），叠加亮度缓慢正弦变化（供 AE 观察）
/// - 左上角 32 位二进制帧计数条（黑/白 4x4 块，直观验证「帧在前进」）
/// - 按 seq 平移的白色方块（运动）
/// - 右上角时间戳低 20 位（二进制条，验证时间戳随帧变化）
pub fn render_test_frame(dst: &mut [u8], result: &CaptureResult) {
    let w = result.width;
    let h = result.height;
    match result.format {
        PixelFormat::Yuv420 => render_yuv420(dst, w, h, result.seq, result.ts_us),
        PixelFormat::Rgb888 => {
            // RGB888：先生成 YUV420 再转换（帧按需渲染，临时缓冲用栈/调用方提供
            // 的转换函数；此处直接生成彩色条更省事，见 render_rgb888）。
            render_rgb888(dst, w, h, result.seq, result.ts_us);
        }
        PixelFormat::RawBayer8 => render_raw8(dst, w, h, result.seq),
    }
}

fn render_yuv420(dst: &mut [u8], w: usize, h: usize, seq: u32, ts_us: u64) {
    let (y_plane, rest) = dst.split_at_mut(w * h);
    let (cb_plane, cr_plane) = rest.split_at_mut((w / 2) * (h / 2));
    let cw = w / 2;
    let ch = h / 2;


    // 运动白块位置
    let sq_x = ((seq as usize * 9) % w.max(1)).min(w.saturating_sub(24));
    let sq_y = h / 3;

    for y in 0..h {
        for x in 0..w {
            let band = (x * 7 / w.max(1)) as u8;
            let (br, bc) = band_brightness(band, y, h, seq);
            // 计数条（左上 4 行）
            let in_counter = y < 4 && x < 128;
            // 时间戳条（右上 4 行）
            let in_ts = y < 4 && x >= w.saturating_sub(128);
            let in_sq = x >= sq_x && x < sq_x + 24 && y >= sq_y && y < sq_y + 24;

            let yv = if in_counter {
                let bit = (seq >> (x / 4)) & 1;
                if bit == 1 {
                    235
                } else {
                    16
                }
} else if in_ts {
                // 简化：ts 低 16 位，自左向右。128px/4px每比特 = 32 位，
                // 移位量应为 31 - idx/4（此前 15 - idx/4 在 idx ≥ 64 时
                // usize 下溢：overflow-checks 构建每帧 panic，release 渲染
                // 错误比特）；基址 saturating（与条件行一致）。
                let idx = x.saturating_sub(w.saturating_sub(128));
                let bitv = (ts_us as u32 >> (31 - (idx / 4).min(31))) & 1;
                if bitv == 1 {
                    200
                } else {
                    32
                }
            } else if in_sq {
                235
            } else {
                // 彩条亮度 + 垂直渐变
                let grad = 255 - y as u32 * 90 / h as u32;
                let b = br as u32;
                ((b * 3 + grad * 2) / 5).clamp(16, 235) as u8
            };
            y_plane[y * w + x] = yv;
            let _ = bc;
        }
    }

    // 色度平面：按彩条填充（黑白区域为中性 128）
    for cy in 0..ch {
        for cx in 0..cw {
            let px = cx * 2;
            let py = cy * 2;
            let band = (px * 7 / w.max(1)) as u8;
            let (cb, cr) = band_chroma(band);
            let neutral = (py < 8) || (px >= w.saturating_sub(132))
                || (px >= ((seq as usize * 9) % w.max(1)).min(w.saturating_sub(24))
                    && px < ((seq as usize * 9) % w.max(1)).min(w.saturating_sub(24)) + 24
                    && py >= h / 3
                    && py < h / 3 + 24);
            cb_plane[cy * cw + cx] = if neutral { 128 } else { cb };
            cr_plane[cy * cw + cx] = if neutral { 128 } else { cr };
        }
    }
}

/// 彩条亮度（每段不同基准亮度）。
fn band_brightness(band: u8, _y: usize, h: usize, _seq: u32) -> (u8, u8) {
    // 亮度随行渐变 + 按带微调，返回 (亮度基准, 备用)
    let base: u32 = match band {
        0 => 180,
        1 => 200,
        2 => 120,
        3 => 220,
        4 => 140,
        5 => 160,
        _ => 128,
    };
    let grad = 255 - _y as u32 * 80 / h.max(1) as u32;
    (((base * 3 + grad * 2) / 5) as u8, 0)
}

/// 彩条色度（BT.601 色度值）。
fn band_chroma(band: u8) -> (u8, u8) {
    match band {
        0 => (90, 240),   // 红
        1 => (54, 34),    // 绿
        2 => (240, 118),  // 蓝
        3 => (16, 146),   // 黄
        4 => (166, 16),   // 青
        5 => (84, 202),   // 品红
        _ => (128, 128),  // 灰
    }
}

/// RGB888 版本：直接生成彩条 + 运动块（供 JPEG 编码链路）。
fn render_rgb888(dst: &mut [u8], w: usize, h: usize, seq: u32, _ts_us: u64) {
    let sq_x = ((seq as usize * 9) % w.max(1)).min(w.saturating_sub(24));
    let sq_y = h / 3;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            let band = (x * 7 / w.max(1)) as u8;
            let (r, g, b) = band_rgb(band);
            let in_counter = y < 4 && x < 128;
            let in_sq = x >= sq_x && x < sq_x + 24 && y >= sq_y && y < sq_y + 24;
            if in_counter {
                let bit = (seq >> (x / 4)) & 1;
                let v = if bit == 1 { 255 } else { 0 };
                dst[i] = v;
                dst[i + 1] = v;
                dst[i + 2] = v;
            } else if in_sq {
                dst[i] = 255;
                dst[i + 1] = 255;
                dst[i + 2] = 255;
            } else {
                dst[i] = r;
                dst[i + 1] = g;
                dst[i + 2] = b;
            }
        }
    }
}

fn band_rgb(band: u8) -> (u8, u8, u8) {
    match band {
        0 => (255, 0, 0),
        1 => (0, 255, 0),
        2 => (0, 0, 255),
        3 => (255, 255, 0),
        4 => (0, 255, 255),
        5 => (255, 0, 255),
        _ => (128, 128, 128),
    }
}

/// RAW8（Bayer RGGB）版本：彩条 + 运动块 + 计数条。
fn render_raw8(dst: &mut [u8], w: usize, h: usize, seq: u32) {
    let sq_x = ((seq as usize * 9) % w.max(1)).min(w.saturating_sub(24));
    let sq_y = h / 3;
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let band = (x * 7 / w.max(1)) as u8;
            let (r, g, b) = band_rgb(band);
            // 简化的 RGGB 采样：奇偶行取 R/G 或 G/B
            let v = if y % 2 == 0 {
                if x % 2 == 0 {
                    r
                } else {
                    g
                }
            } else if x % 2 == 0 {
                g
            } else {
                b
            };
            let in_sq = x >= sq_x && x < sq_x + 24 && y >= sq_y && y < sq_y + 24;
            let in_counter = y < 4 && x < 128;
            dst[i] = if in_sq {
                255
            } else if in_counter {
                if (seq >> (x / 4)) & 1 == 1 {
                    235
                } else {
                    16
                }
            } else {
                v
            };
        }
    }
}

// （相机真实实现见上文「相机：软件模拟传感器（SimCamera）」一节，
//  占位 static 已由真实 `SIM_CAMERA` 取代。）

// ==================== Wi-Fi：软件模拟后端（SimWifi） ====================
//
// QEMU virt 无真实无线硬件。SimWifi 实现与 `kunxwireless` 一致的 Wi-Fi 行为，
// 使 QEMU 下可以端到端验证「扫描 → 关联 → WPA2-PSK 4-way handshake → 已连接」
// 整条链路：
//   1. 扫描：确定性 AP 表（开放 / WPA2-PSK ×2 / WPA3 演示不支持）。
//   2. 关联：按 BSSID 关联，链路进入 Associated。
//   3. 4-way handshake：**AP 侧真实执行**（复用 `kunxwireless::eapol::FourWay`），
//      STA 侧由内核 `kunxcore::wifi` 用同一密码学库执行 —— 口令正确则双方收敛
//      到同一 PTK、MIC 互验通过、链路进入 Secured；口令错误则 AP 侧 MIC 校验
//      失败，链路进入 Failed。QEMU 下即可验证 WPA2-PSK 密码学行为。
//   4. 租约：直接返回与 QEMU slirp 一致的静态租约（10.0.2.15/24），
//      保证模拟「已连接 + 可上网」时内核网络栈配置不被破坏（真实后端走线上 DHCP）。

use crate::wifi::{
    ETH_TYPE_EAPOL, WifiAuth, WifiDevice, WifiError, WifiLinkState, WifiScanPoll, WifiScanResult,
    WifiStatus,
};

/// 模拟 AP 数量。
pub const SIM_AP_COUNT: usize = 4;

/// 模拟 AP 配置（BSSID / SSID / 信道 / 信号 / 认证 / 口令）。
#[derive(Clone, Copy)]
pub struct SimApCfg {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub bssid: [u8; 6],
    pub channel: u8,
    pub rssi: i8,
    pub auth: WifiAuth,
    pub hidden: bool,
    pub psk: [u8; 64],
    pub psk_len: u8,
}

/// 常量构造器（const fn：`ssid`/`psk` 不超过上限时原样复制）。
const fn ap_cfg(
    ssid: &[u8],
    bssid: [u8; 6],
    channel: u8,
    rssi: i8,
    auth: WifiAuth,
    hidden: bool,
    psk: &[u8],
) -> SimApCfg {
    let mut c = SimApCfg {
        ssid: [0; 32],
        // 长度字段必须按实际拷贝上限截断（消费者按 ssid_len 切片，超长
        // 长度字段会让 ssid[..len] 越界 panic）。const fn 无 Ord::min，
        // 用条件表达式。
        ssid_len: if ssid.len() > 32 { 32 } else { ssid.len() } as u8,
        bssid,
        channel,
        rssi,
        auth,
        hidden,
        psk: [0; 64],
        psk_len: if psk.len() > 64 { 64 } else { psk.len() } as u8,
    };
    let mut i = 0;
    while i < ssid.len() && i < 32 {
        c.ssid[i] = ssid[i];
        i += 1;
    }
    let mut j = 0;
    while j < psk.len() && j < 64 {
        c.psk[j] = psk[j];
        j += 1;
    }
    c
}

/// 确定性 AP 表：KunxHome（WPA2-PSK，默认演示网络）/
/// KunxGuest（开放） / Kunx-IoT-5G（隐藏 WPA2-PSK） / KunxOffice（WPA3，不支持）。
pub static SIM_APS: [SimApCfg; SIM_AP_COUNT] = [
    ap_cfg(b"KunxHome", [0x02, 0x00, 0x00, 0x00, 0x00, 0x01], 6, -45, WifiAuth::Wpa2Psk, false, b"kunxhome123"),
    ap_cfg(b"KunxGuest", [0x02, 0x00, 0x00, 0x00, 0x00, 0x02], 1, -62, WifiAuth::Open, false, b""),
    ap_cfg(b"Kunx-IoT-5G", [0x02, 0x00, 0x00, 0x00, 0x00, 0x03], 11, -70, WifiAuth::Wpa2Psk, true, b"iot2026!net"),
    ap_cfg(b"KunxOffice", [0x02, 0x00, 0x00, 0x00, 0x00, 0x04], 36, -78, WifiAuth::Wpa3Sae, false, b""),
];

/// 模拟 AP 的 4-way 状态（AP 侧，真实密码学）。
#[derive(Clone, Copy)]
pub struct SimAp {
    /// 当前是否已关联（有活动会话）。
    pub active: bool,
    pub cfg: SimApCfg,
    /// STA 地址（关联时由内核传入，用作帧目的地址与 PTK 派生的 SA）。
    pub sta_mac: [u8; 6],
    /// AP 自己的 ANonce / 从 Msg2 学到的 SNonce。
    pub anonce: [u8; 32],
    pub snonce: [u8; 32],
    pub replay: u64,
    /// PMK（由 AP 口令 + SSID 经 PBKDF2 派生）。
    pub pmk: [u8; 32],
    /// AP 侧 4-way 状态机（kunxwireless::eapol::FourWay）。
    pub apfw: kunxwireless::eapol::FourWay,
    /// AP 生成的 GTK（Msg3 用 KEK 包装后下发）。
    pub gtk: [u8; 32],
    /// AP → STA 出队帧缓冲（环，每条完整以太网帧 ≤ 300 字节）。
    pub txq: [[u8; 320]; SIM_AP_COUNT],
    pub txq_len: [usize; SIM_AP_COUNT],
    pub txq_head: usize,
    pub txq_count: usize,
}

impl SimAp {
    pub const fn empty() -> Self {
        SimAp {
            active: false,
            cfg: ap_cfg(b"", [0; 6], 0, 0, WifiAuth::Open, false, b""),
            sta_mac: [0; 6],
            anonce: [0; 32],
            snonce: [0; 32],
            replay: 0,
            pmk: [0; 32],
            apfw: kunxwireless::eapol::FourWay {
                pmk: [0; 32],
                aa: [0; 6],
                sa: [0; 6],
                anonce: [0; 32],
                snonce: [0; 32],
                replay_counter: 0,
                ptk: kunxwireless::prf::Ptk { kck: [0; 16], kek: [0; 16], tk: [0; 32] },
                kdv: kunxwireless::eapol::KDV_HMAC_SHA1,
            },
            gtk: [0; 32],
            txq: [[0; 320]; SIM_AP_COUNT],
            txq_len: [0; SIM_AP_COUNT],
            txq_head: 0,
            txq_count: 0,
        }
    }

    /// 入队一条 AP → STA 帧；队列满时丢弃（模拟丢包，供链路层观察）。
    fn tx_push(&mut self, frame: &[u8]) {
        if self.txq_count >= SIM_AP_COUNT || frame.len() > 320 {
            return;
        }
        let slot = (self.txq_head + self.txq_count) % SIM_AP_COUNT;
        self.txq[slot][..frame.len()].copy_from_slice(frame);
        self.txq_len[slot] = frame.len();
        self.txq_count += 1;
    }

    /// 出队一条 AP → STA 帧。
    fn tx_pop(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.txq_count == 0 {
            return None;
        }
        let len = self.txq_len[self.txq_head];
        let n = len.min(out.len());
        out[..n].copy_from_slice(&self.txq[self.txq_head][..n]);
        self.txq_head = (self.txq_head + 1) % SIM_AP_COUNT;
        self.txq_count -= 1;
        Some(n)
    }

    /// 构造一条以太网帧：dst/sat/mac、ethertype 0x888E、载荷 EAPOL。
    fn eth_wrap(&self, dst: &[u8; 6], etype: u16, payload: &[u8], out: &mut [u8]) -> usize {
        let total = 14 + payload.len();
        if total > out.len() {
            return 0;
        }
        out[0..6].copy_from_slice(dst);
        out[6..12].copy_from_slice(&self.cfg.bssid);
        out[12] = (etype >> 8) as u8;
        out[13] = (etype & 0xFF) as u8;
        out[14..total].copy_from_slice(payload);
        total
    }
}

/// 读一条完整以太网帧的 ethertype（<14 返回 0）。
fn eth_etype(frame: &[u8]) -> u16 {
    if frame.len() < 14 {
        return 0;
    }
    u16::from_be_bytes([frame[12], frame[13]])
}

#[allow(dead_code)]
fn eth_src(frame: &[u8]) -> [u8; 6] {
    let mut m = [0u8; 6];
    if frame.len() >= 12 {
        m.copy_from_slice(&frame[6..12]);
    }
    m
}

/// QEMU 软件 Wi-Fi 后端单例。
pub static mut SIM_WIFI: SimWifi = SimWifi::new();

/// 获取 SimWifi 的可变引用（内核 / 平台后端调用；单核 + 中断屏蔽）。
pub fn sim_wifi() -> &'static mut SimWifi {
    unsafe { &mut *core::ptr::addr_of_mut!(SIM_WIFI) }
}

/// 软件 Wi-Fi 网卡后端的可变状态。
pub struct SimWifi {
    /// STA 固定 MAC（模拟网卡的 54:00 OUI 段，与 virtio 网卡区分）。
    mac: [u8; 6],
    /// 扫描进度。
    scan_index: usize,
    scan_done: bool,
    scan_started: bool,
    /// 链路状态。
    link: WifiLinkState,
    /// 当前 AP 会话。
    ap: SimAp,
    /// 最近关联 AP 的信号强度（状态快照用）。
    rssi: i8,
}

impl SimWifi {
    pub const fn new() -> Self {
        SimWifi {
            mac: [0x52, 0x54, 0x00, 0x00, 0x00, 0x02],
            scan_index: 0,
            scan_done: false,
            scan_started: false,
            link: WifiLinkState::Idle,
            ap: SimAp::empty(),
            rssi: 0,
        }
    }

    /// 立即完成整次扫描（QEMU 模拟，无真实射频时延）。
    #[allow(dead_code)]
    fn finish_scan(&mut self) {
        self.scan_index = SIM_AP_COUNT;
        self.scan_done = true;
    }

    /// 查找 BSSID 对应的 AP 配置。
    fn find_ap(&self, bssid: &[u8; 6]) -> Option<SimApCfg> {
        SIM_APS.iter().find(|a| &a.bssid == bssid).copied()
    }
}

impl WifiDevice for SimWifi {
    fn name(&self) -> &'static str {
        "sim-wifi0"
    }

    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn available(&self) -> bool {
        // QEMU 模拟后端永远「存在」。
        true
    }

    fn start_scan(&mut self) -> bool {
        self.scan_started = true;
        self.scan_index = 0;
        self.scan_done = false;
        true
    }

    fn poll_scan(&mut self, out: &mut [WifiScanResult]) -> WifiScanPoll {
        if !self.scan_started {
            return WifiScanPoll { done: false, count: 0 };
        }
        let mut n = 0usize;
        while self.scan_index < SIM_AP_COUNT && n < out.len() {
            let a = &SIM_APS[self.scan_index];
            let mut r = WifiScanResult::empty();
            r.ssid[..a.ssid_len as usize].copy_from_slice(&a.ssid[..a.ssid_len as usize]);
            r.ssid_len = a.ssid_len;
            r.bssid = a.bssid;
            r.channel = a.channel;
            r.rssi = a.rssi;
            r.auth = a.auth;
            r.hidden = a.hidden;
            out[n] = r;
            n += 1;
            self.scan_index += 1;
        }
        if self.scan_index >= SIM_AP_COUNT {
            self.scan_done = true;
        }
        WifiScanPoll { done: self.scan_done, count: n }
    }

    fn associate(
        &mut self,
        bssid: [u8; 6],
        sta_mac: [u8; 6],
    ) -> Result<(), WifiError> {
        let Some(cfg) = self.find_ap(&bssid) else {
            return Err(WifiError::NotFound);
        };
        if cfg.auth == WifiAuth::Wpa3Sae || cfg.auth == WifiAuth::Dot1x {
            return Err(WifiError::Unsupported);
        }
        // 开放网络直接进入 Secured（无 4-way）；WPA2-PSK 先 Associated。
        let open = cfg.auth == WifiAuth::Open;
        self.ap.active = true;
        self.ap.cfg = cfg;
        self.ap.sta_mac = sta_mac;
        self.rssi = cfg.rssi;
        self.link = if open { WifiLinkState::Secured } else { WifiLinkState::Associating };
        if open {
            // 开放网络：无需握手，直接可传数据。
            return Ok(());
        }
        // ---- WPA2-PSK：初始化 AP 侧 4-way ----
        let dk = kunxwireless::pbkdf2::pbkdf2_sha1(
            &self.ap.cfg.psk[..self.ap.cfg.psk_len as usize],
            &self.ap.cfg.ssid[..self.ap.cfg.ssid_len as usize],
            4096,
            32,
        );
        let mut pmk = [0u8; 32];
        pmk.copy_from_slice(&dk[..32]);
        self.ap.pmk = pmk;
        self.ap.anonce = [0x44; 32]; // 模拟 ANonce（确定性，可复现）
        self.ap.gtk = [0x55; 32]; // 模拟 GTK
        self.ap.replay = 1;
        self.ap.snonce = [0; 32];
        self.ap.apfw = kunxwireless::eapol::FourWay {
            pmk,
            aa: cfg.bssid,
            sa: sta_mac,
            anonce: self.ap.anonce,
            snonce: [0; 32],
            replay_counter: 1,
            ptk: kunxwireless::prf::Ptk { kck: [0; 16], kek: [0; 16], tk: [0; 32] },
            kdv: kunxwireless::eapol::KDV_HMAC_SHA1,
        };
        // 构造 Msg1（ANonce）并排队发给 STA。
        let mut body = [0u8; 300];
        let n = self.ap.apfw.ap_build_msg1(&mut body)
            .unwrap_or(0);
        let mut frame = [0u8; 320];
        let fl = self.ap.eth_wrap(&self.ap.sta_mac, ETH_TYPE_EAPOL, &body[..n], &mut frame);
        if fl > 0 {
            self.ap.tx_push(&frame[..fl]);
        }
        self.link = WifiLinkState::Associated;
        Ok(())
    }

    fn disassociate(&mut self) {
        self.ap.active = false;
        self.ap.txq_count = 0;
        self.ap.txq_head = 0;
        self.link = WifiLinkState::Idle;
        self.rssi = 0;
    }

    fn link_state(&self) -> WifiLinkState {
        self.link
    }

    fn recv_eapol(&mut self, out: &mut [u8]) -> Option<usize> {
        // 出队 AP → STA 的 EAPOL 帧。
        self.ap.tx_pop(out)
    }

    fn send_eapol(&mut self, frame: &[u8]) -> bool {
        if eth_etype(frame) != ETH_TYPE_EAPOL || frame.len() < 14 + 4 + 95 {
            return false;
        }
        let payload = &frame[14..];
        let Some(msg) = kunxwireless::eapol::EapolKey::parse(payload) else {
            return false;
        };
        use kunxwireless::eapol::{classify, compute_mic, HandshakeMsg, keyinfo};
        match classify(msg.key_info) {
            HandshakeMsg::Msg2 => {
                // STA 已发送 SNonce：派生双方 PTK，校验 MIC。
                self.ap.snonce = msg.key_nonce;
                let ptk = kunxwireless::prf::derive_ptk(
                    &self.ap.pmk,
                    &self.ap.apfw.aa,
                    &self.ap.apfw.sa,
                    &self.ap.anonce,
                    &msg.key_nonce,
                );
                self.ap.apfw.ptk = kunxwireless::prf::Ptk::from_ptk(&ptk);
                if !self.ap.apfw.ap_verify_msg2(&msg, payload) {
                    self.link = WifiLinkState::Failed;
                    return true; // 口令错误的典型症状：Msg2 MIC 校验不过
                }
                // 构造 Msg3（GTK 经 KEK 包装 + RSN IE）。
                let rsn = kunxwireless::wifi_ie::build_rsn_ie(
                    kunxwireless::wifi_ie::CIPHER_CCMP,
                    kunxwireless::wifi_ie::AKM_PSK,
                    0,
                );
                let mut body = [0u8; 320];
                let n = self.ap.apfw.ap_build_msg3(&self.ap.gtk, &rsn, &mut body)
                    .unwrap_or(0);
                let mut f3 = [0u8; 320];
                let fl = self.ap.eth_wrap(&self.ap.sta_mac, ETH_TYPE_EAPOL, &body[..n], &mut f3);
                if fl > 0 {
                    self.ap.tx_push(&f3[..fl]);
                }
                true
            }
            HandshakeMsg::Msg4 => {
                // 最后一条：校验 Msg4 MIC，成功则双方密钥安装完成。
                let mic = compute_mic(&self.ap.apfw.ptk.kck, payload, keyinfo::kdv(msg.key_info));
                if mic == msg.key_mic {
                    self.link = WifiLinkState::Secured;
                } else {
                    self.link = WifiLinkState::Failed;
                }
                true
            }
            _ => true, // 其它帧忽略
        }
    }

    fn status(&self) -> WifiStatus {
        WifiStatus {
            link: self.link,
            ssid: self.ap.cfg.ssid,
            ssid_len: self.ap.cfg.ssid_len,
            bssid: self.ap.cfg.bssid,
            rssi: self.rssi,
        }
    }

    fn lease(&self, ip: &mut u32, gw: &mut u32, mask: &mut u32, dns: &mut u32) -> bool {
        // 与 QEMU slirp 一致的静态租约：确保模拟「已连接」后网络栈不损坏。
        *ip = 0x0A00_020F; // 10.0.2.15
        *gw = 0x0A00_0202; // 10.0.2.2
        *mask = 0xFFFF_FF00; // 255.255.255.0
        *dns = 0x0A00_0203; // 10.0.2.3
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端验证：SimWifi（AP 侧）+ STA 侧 4-way（kunxwireless，与内核同库），
    /// 口令正确收敛到 Secured、MIC 互验通过；口令错误必须在 Msg2 被 AP 拒绝。
    #[test]
    fn sim_ap_four_way_roundtrip() {
        // STA 侧状态（与内核 kunxcore::wifi 完全相同的代码路径）。
        let mut sta = SimWifi::new();
        let _ = sta.associate([0x02, 0x00, 0x00, 0x00, 0x00, 0x01], [0x52, 0x54, 0x00, 0x00, 0x00, 0x02]);
        assert_eq!(sta.link_state(), WifiLinkState::Associated);

        // 内核会：收到 Msg1 → 派生 PTK → 发 Msg2。此处用 kunxwireless 复刻。
        let mut pmk = [0u8; 32];
        let dk = kunxwireless::pbkdf2::pbkdf2_sha1(b"kunxhome123", b"KunxHome", 4096, 32);
        pmk.copy_from_slice(&dk[..32]);
        let sa = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
        let aa = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let snonce = [0x22u8; 32];

        let mut hw = kunxwireless::eapol::FourWay {
            pmk,
            aa,
            sa,
            anonce: [0; 32],
            snonce,
            replay_counter: 0,
            ptk: kunxwireless::prf::Ptk::from_ptk(&[0u8; 64]),
            kdv: kunxwireless::eapol::KDV_HMAC_SHA1,
        };

        // 消息1 → STA
        let mut f1 = [0u8; 320];
        let n1 = sta.recv_eapol(&mut f1).expect("ap pushes msg1");
        let msg1 = kunxwireless::eapol::EapolKey::parse(&f1[14..n1]).unwrap();
        hw.sta_on_msg1(&msg1).unwrap();
        hw.anonce = msg1.key_nonce;

        // 消息2 → AP
        let rsn = kunxwireless::wifi_ie::build_rsn_ie(
            kunxwireless::wifi_ie::CIPHER_CCMP,
            kunxwireless::wifi_ie::AKM_PSK,
            0,
        );
        let mut body = [0u8; 320];
        let nb = hw.sta_build_msg2(&rsn, &mut body).unwrap();
        let mut f2 = [0u8; 320];
        f2[..6].copy_from_slice(&aa);
        f2[6..12].copy_from_slice(&sa);
        f2[12..14].copy_from_slice(&ETH_TYPE_EAPOL.to_be_bytes());
        f2[14..14 + nb].copy_from_slice(&body[..nb]);
        assert!(sta.send_eapol(&f2[..14 + nb]));

        // 消息3 → STA
        let mut f3 = [0u8; 320];
        let n3 = sta.recv_eapol(&mut f3).expect("ap pushes msg3");
        let msg3 = kunxwireless::eapol::EapolKey::parse(&f3[14..n3]).unwrap();
        let mut gtk = [0u8; 32];
        let _kid = hw.sta_on_msg3(&msg3, &mut gtk).expect("unwrap gtk");
        assert_eq!(&gtk, &[0x55u8; 32]);

        // 消息4 → AP
        let mut body4 = [0u8; 320];
        let nb4 = hw.sta_build_msg4(&mut body4).unwrap();
        let mut f4 = [0u8; 320];
        f4[..6].copy_from_slice(&aa);
        f4[6..12].copy_from_slice(&sa);
        f4[12..14].copy_from_slice(&ETH_TYPE_EAPOL.to_be_bytes());
        f4[14..14 + nb4].copy_from_slice(&body4[..nb4]);
        assert!(sta.send_eapol(&f4[..14 + nb4]));

        assert_eq!(sta.link_state(), WifiLinkState::Secured);
        assert_eq!(sta.status().rssi, -45);
    }

    #[test]
    fn sim_ap_rejects_wrong_psk() {
        let mut sta = SimWifi::new();
        let _ = sta.associate([0x02, 0x00, 0x00, 0x00, 0x00, 0x01], [0x52, 0x54, 0x00, 0x00, 0x00, 0x02]);
        let mut pmk = [0u8; 32];
        // 错误口令。
        let dk = kunxwireless::pbkdf2::pbkdf2_sha1(b"wrong-password", b"KunxHome", 4096, 32);
        pmk.copy_from_slice(&dk[..32]);
        let sa = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
        let aa = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let snonce = [0x22u8; 32];
        let mut hw = kunxwireless::eapol::FourWay {
            pmk, aa, sa,
            anonce: [0; 32],
            snonce,
            replay_counter: 0,
            ptk: kunxwireless::prf::Ptk::from_ptk(&[0u8; 64]),
            kdv: kunxwireless::eapol::KDV_HMAC_SHA1,
        };
        let mut f1 = [0u8; 320];
        let n1 = sta.recv_eapol(&mut f1).unwrap();
        let msg1 = kunxwireless::eapol::EapolKey::parse(&f1[14..n1]).unwrap();
        hw.sta_on_msg1(&msg1).unwrap();
        hw.anonce = msg1.key_nonce;
        let rsn = kunxwireless::wifi_ie::build_rsn_ie(
            kunxwireless::wifi_ie::CIPHER_CCMP,
            kunxwireless::wifi_ie::AKM_PSK,
            0,
        );
        let mut body = [0u8; 320];
        let nb = hw.sta_build_msg2(&rsn, &mut body).unwrap();
        let mut f2 = [0u8; 320];
        f2[..6].copy_from_slice(&aa);
        f2[6..12].copy_from_slice(&sa);
        f2[12..14].copy_from_slice(&ETH_TYPE_EAPOL.to_be_bytes());
        f2[14..14 + nb].copy_from_slice(&body[..nb]);
        assert!(sta.send_eapol(&f2[..14 + nb]));
        // 错误的 PMK → AP 侧 Msg2 MIC 校验失败 → Failed。
        assert_eq!(sta.link_state(), WifiLinkState::Failed);
    }

    #[test]
    fn sim_scan_lists_all_aps() {
        let mut w = SimWifi::new();
        assert!(w.start_scan());
        let mut out = [WifiScanResult::empty(); 16];
        let p = w.poll_scan(&mut out);
        assert!(p.done);
        assert_eq!(p.count, SIM_AP_COUNT);
        assert_eq!(&out[0].ssid[..out[0].ssid_len as usize], b"KunxHome");
        assert_eq!(out[0].auth, WifiAuth::Wpa2Psk);
        assert!(out[3].auth == WifiAuth::Wpa3Sae);
    }
}
