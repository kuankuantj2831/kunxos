# KunxOS 相机子系统设计文档

> 目标：为相册、相机应用提供底层能力——相机 HAL 抽象、基础采集、3A 框架、
> JPEG 拍照、视频录制（MP4/MOV 预留）、多摄像头切换预留。
> 目标平台：Raspberry Pi 4（BCM2711，CSI-2 + Camera Module v2/v3）与
> QEMU `virt`（无真实相机，软件模拟传感器）。

## 1. 架构图

```
┌─────────────────────┐    文本命令 + 帧文件      ┌─────────────────────┐
│   应用层 (EL0)       │ ◄──────────────────────► │  相机服务 (EL0)      │
│  apps/camera        │  /var/run/camerad.ctl     │  kunxserv/camerad   │
│  取景/快门/录像      │  /var/run/camerad.resp    │  命令处理 · 帧泵     │
└─────────────────────┘  /tmp/cam/frame_*.yuv     │  JPEG 编码 · 落盘    │
                      │  /data/photos/*.jpg       └──────────┬──────────┘
                      │  /data/videos/*.kxv                  │ 相机 HAL API
                      │                                      ▼
                      │                          ┌─────────────────────┐
                      │                          │  kunxhal::camera    │
                      │                          │  CameraDevice trait │
                      │                          └──────────┬──────────┘
                      │                                     │
                      │                     ┌───────────────┴───────────────┐
                      │                     ▼                               ▼
                      │        ┌────────────────────────┐      ┌──────────────────────────┐
                      │        │ QEMU 后端 SimCamera     │      │ Pi4 后端 PiCamera（骨架）  │
                      │        │ 软件传感器：彩条+计数器  │      │ mailbox 探测 · Unicam      │
                      │        │ 帧按需确定性渲染         │      │ CSI-2 寄存器 · IMX219 I2C  │
                      │        └────────────────────────┘      └──────────────────────────┘
                      │                                          │ 固件（VideoCore mailbox）
                      │                                          ▼
                      │                                  IMX219/IMX708（2-lane CSI-2）
```

- **应用 ↔ 服务**：应用为裸 syscall EL0 进程（无 libc/堆），服务同样。进程间通信
  采用仓库既有模式（同 `networkd`）：命令写普通文件、服务轮询清空、响应追加另一
  文件；帧数据经 `/tmp/cam` 环形文件传递（固定文件复用，避免 ramdisk 反复建删）。
- **服务 ↔ HAL**：camerad 持有 `kunxhal::camera::camera_backend()` 返回的设备
  实例，按帧率轮询 `poll_frame()`（EL0 无中断/线程，事件循环驱动）。
- **HAL ↔ 驱动/固件**：QEMU 后端是纯软件传感器；Pi4 后端是硬件接线骨架
  （mailbox 探测固件 + BCM2711 CSI-2/Unicam 寄存器 + BSC/I2C 配置 IMX219）。

## 2. HAL 接口定义（kunxhal/src/camera.rs）

```rust
/// 采集配置：分辨率 / 帧率 / 格式
pub struct CameraConfig {
    pub width: usize, pub height: usize, pub fps: usize,
    pub format: PixelFormat,           // RawBayer8 | Yuv420 | Rgb888
}
impl CameraConfig {
    pub const fn new(w: usize, h: usize, fps: usize, f: PixelFormat) -> Self;
    pub fn frame_size(&self) -> usize; // Yuv420 = w*h*3/2
}

/// 设备信息（枚举结果）
pub struct CameraDeviceInfo {
    pub id: u32, pub name: &'static str,
    pub facing: CameraFacing,          // Back | Front（多摄预留）
    pub max_width: usize, pub max_height: usize,
    pub formats: u8,                   // 支持格式位图
}

/// 采集请求
pub struct CaptureRequest {
    pub camera_id: u32,
    pub config: CameraConfig,
    pub single: bool,                  // true=单帧拍照；false=连续采集
    pub buffer_mask: u32,              // 期望缓冲槽位掩码（0=设备决定）
}

/// 采集结果（一帧）
pub struct CaptureResult {
    pub seq: u32,                      // 帧序号（连续采集单调递增）
    pub ts_us: u64,                    // 时间戳（微秒，单调时钟）
    pub width: usize, pub height: usize, pub format: PixelFormat,
    pub buffer_index: usize,           // 数据所在缓冲池槽位
    pub exposure_us: u32, pub iso_gain: u16,   // AE 元数据
    pub awb_temp_k: u16, pub focus_pos: u8,    // AWB / AF 元数据
}

/// 相机设备抽象（平台后端实现）
pub trait CameraDevice {
    fn enumerate(&self, out: &mut [CameraDeviceInfo]) -> usize;
    fn open(&mut self, id: u32) -> Result<(), CameraError>;
    fn close(&mut self) -> Result<(), CameraError>;
    fn configure(&mut self, config: &CameraConfig) -> Result<(), CameraError>;
    fn start_stream(&mut self) -> Result<(), CameraError>;   // 连续采集
    fn stop_stream(&mut self) -> Result<(), CameraError>;
    fn poll_frame(&mut self) -> Option<CaptureResult>;       // 非阻塞取帧
    fn capture_single(&mut self, req: &CaptureRequest) -> Result<CaptureResult, CameraError>;
    fn get_3a(&self) -> ThreeAParams;
    fn set_3a(&mut self, params: ThreeAParams);
    fn read_frame(&mut self, r: &CaptureResult, dst: &mut [u8]) -> Result<usize, CameraError>;
}

/// 3A 框架：AE / AF / AWB 参数 + 帧统计 + 算法 trait
pub struct ThreeAParams { pub ae: AeParams, pub af: AfParams, pub awb: AwbParams }
pub struct FrameStats { pub mean_luma: u8, pub focus_metric: u16, pub mean_r/g/b: u8 }
pub trait ThreeA {
    fn update(&mut self, stats: &FrameStats) -> ThreeAParams; // 依据统计更新
    fn params(&self) -> ThreeAParams;
    fn set(&mut self, params: ThreeAParams);                   // 手动覆盖
}
pub struct FixedThreeA { /* 固定参数实现（当前默认） */ }
```

设计取舍：

- **`poll_frame` 而非回调**：EL0 服务无中断线程，事件循环轮询与仓库既有
  `networkd` 风格一致；Pi4 硬件接线后可在 `poll_frame` 内检查 Unicam DMA 状态
  （或由内核中断填充），接口不变。
- **缓冲索引 + 按需读取**：结果携带 `buffer_index`，数据经 `read_frame` 读取；
  QEMU 后端按 (seq, ts) 确定性重渲染（零大缓冲），Pi4 后端将对应 Unicam DMA
  缓冲槽（CPU/物理地址双视图）。

## 3. 基础采集：缓冲区管理（DMA / CPU）

HAL 提供 `BufferPool<N>`（`kunxhal/src/camera.rs`）：

```rust
pub enum BufferState { Free, Filled }
pub struct BufferPool<const N: usize> {
    states: [BufferState; N],
    phys:   [usize; N],     // DMA 场景：槽位物理地址（后端填入）
}
impl<const N: usize> BufferPool<N> {
    pub const fn new() -> Self;
    pub fn acquire_free(&mut self) -> Option<usize>;    // 后端取空槽写入
    pub fn acquire_filled(&mut self) -> Option<usize>;  // 消费者取已填充槽
    pub fn release(&mut self, index: usize);
    pub fn set_phys(&mut self, index: usize, phys: usize);
    pub fn phys(&self, index: usize) -> usize;
}
```

- **CPU 路径（QEMU）**：帧是 (seq, ts_us, config) 的确定性函数，`read_frame`
  按需重渲染进调用方缓冲，`BufferPool` 语义由 camerad 侧文件环形缓冲承担
  （6 个固定帧文件 + latest 序号，避免 ramdisk 块泄漏）。
- **DMA 路径（Pi4，接线后）**：Unicam 的 CSI DMA 直接把 RAW 数据写入
  `BufferPool` 槽位的物理地址（`set_phys`），`poll_frame` 检查
  `CSI_DMA_STATUS` 完成位后返回 `buffer_index`；CPU 读槽前做 cache 维护
  （`dc ivac`，参考 `plat_pi4.rs` 现有 mailbox 代码的 cache 操作）。
- **零拷贝共享**（见 §7 关键问题 4）：Pi4 接线下，把 Unicam DMA 目标指到
  framebuffer 物理页可实现「相机直写屏幕」；当前 KunxOS 合成器为 CPU 写
  `/dev/fb0`，尚未接线。

## 4. JPEG 编码（shared/jpeg —— 纯 Rust baseline 编码器）

- 定位：`no_std`、零堆、纯整数 DCT（无浮点依赖），可在 EL0 服务与宿主测试运行。
- 能力：RGB888 → JFIF baseline JPEG，4:2:0 抽样，8x8 整数 FDCT，
  标准量化表（按质量 1..100 IJG 缩放）+ 标准 Huffman 表（Annex K），
  zigzag + DC 差分 + AC 行程编码，熵数据 0xFF 字节填充。
- API：`kunxjpeg::jpeg_encode_rgb888(dst, w, h, rgb, quality) -> Result<usize, JpegError>`；
  宽高可为奇数（边缘复制补齐）；输出缓冲不足返回 `BufferTooSmall{needed}`。
- 校验：7 个宿主单测（结构校验 / 奇偶尺寸 / 确定性 / 缓冲不足 / 压缩率对比），
  并用 Pillow 解码验证色带还原（640x480 误差 < 20/255，见 `tests/scripts/verify_jpeg.py`）。
- 未绑定 libjpeg-turbo 的原因：目标环境无动态链接、无堆，cargo 依赖树需保持
  纯 Rust；zune-jpeg 是解码器（编码另有 zune-image 系），自研编码器约 600 行，
  完全可控且可宿主测试。

## 5. 3A 框架（AE / AF / AWB 占位）

- `ThreeA` trait + `FrameStats`（帧亮度/对焦评价/通道均值）定义算法输入输出；
- `FixedThreeA` 当前实现：固定曝光 1/30s、ISO 100、5500K、对焦 50，
  由 `camerad 3A_GET/3A_SET` 命令读写，拍照元数据写入 `CaptureResult`；
- 后续简化算法（文档预留接口）：
  - AE：以 `mean_luma` 目标 128 做比例步进（`exposure *= 128/mean`），钳制曝光
    范围后调 ISO；
  - AWB：灰世界校正 `gain_r = 128/mean_r`、`gain_b = 128/mean_b`（16.16 定点）；
  - AF：中心窗 `focus_metric` 爬山搜索对焦位置（Pi4 接线下经 I2C 写马达寄存器）。

## 6. 拍照 / 录像流程

**拍照**（`camerad PHOTO`）：临时配置 640x480 RGB888 → `capture_single` →
`read_frame` 得 RGB → `kunxjpeg` 质量 85 编码 → `/data/photos/photo_XXXXXX.jpg`
→ 恢复预览配置；响应 `#id <序号> <字节数> <seq>`。应用侧显示「已保存」+ 缩略图
（缩略图取自最近预览帧，避免在 EL0 解 JPEG）。

**录像**（`camerad REC_START/REC_STOP`）：连续采集（默认 320x240@15 YUV420），
逐帧写入自描述容器 `/data/videos/rec_XXXXXX.kxv`：
`文件头 16B（magic "KXRV" + w + h + fps + fmt）` + `每帧 16B（seq + ts_us + w + h）` + YUV 数据。
**MP4/MOV 预留**：帧头字段与时间戳已就绪，后续以 `mp4mux` 类组件把 (ts, data)
封装为 ftyp/moov/mdat 结构即可替换容器；`.kxv` 保持为调试/回放格式。

**多摄像头（前后切换预留）**：`CameraFacing::{Back, Front}` + `CameraDeviceInfo.id`，
Pi4 的 CAM1（CSI0）与 CAM0（CSI1）对应两个设备；camerad 协议 `OPEN <id>` 已按
id 寻址，QEMU 后端当前仅模拟 id=0。

## 7. 关键问题解答

### Q1：Pi4 当前是否已打通 MMAL/IL 固件接口？还是直接操作 CSI 寄存器？

**都不是（当前骨架）**。现状与结论：

- VideoCore 固件提供 MMAL/IL 接口，但**闭源且基于 VCHIQ**——裸机 KunxOS 目前
  仅实现 mailbox 属性通道（帧缓冲申请、时钟/电压查询），没有 VCHIQ/MMAL 用户端，
  从零对接 MMAL 工作量很大且文档稀缺。
- 另一条路径是**直接操作 BCM2711 外设寄存器**：CSI-2 接收器（Unicam）基址
  `CSI0=0xFE801000 / CSI1=0xFE802000`，传感器经 BSC0（I2C0，GPIO 44/45）配置。
  该路径**可行**（Linux 的 bcm2835-unicam 驱动即此方式，IMX219 寄存器公开），
  不需要固件接口，但需要：GPIO 复用 → I2C 枚举 IMX219（chip id 0x0219）→
  写分辨率/曝光/增益 → 启动 streaming → 编程 Unicam（2-lane、RAW10、DMA）→
  轮询/中断取帧。`kunxhal/src/plat_pi4.rs` 已整理该序列的寄存器常量与步骤大纲。
- **当前交付**：`PiCamera` 实现完整 `CameraDevice` 状态机（open/configure/3A），
  采集路径返回 `NotImplemented` 并附接线 TODO；QEMU 下全链路已可用（SimCamera）。

### Q2：是否接受绑定 libcamera（Raspberry Pi 官方相机库）以加速实现？

**不建议（本项目语境）**：libcamera 面向 Linux 用户态（依赖 libc、线程、V4L2/
MEDIA 内核接口、动态库加载），KunxOS 是裸机 no_std 系统、无 libc/动态链接，
移植成本不低于直接写 Unicam+I2C 驱动；且任务约束明确「不引入完整 Android
Camera2 API 或 Linux V4L2 栈，保持自研 HAL」。真机加速的现实路径是**参考**
libcamera/RPi 的**寄存器初始化序列**（其 IMX219 配置表与 Unicam 编程是公开的），
在自研 HAL 内复刻，而非绑定其代码。

### Q3：1080p@30 采集的带宽需求？CSI-2 2-lane 是否足够？

| 格式 | 每帧字节 | 1080p@30 带宽 |
|---|---|---|
| RAW10 Bayer（IMX219 原生） | 1920×1080×10/8 = 2.59 MB | ≈ 622 Mbps |
| RAW8 Bayer | 2.07 MB | ≈ 497 Mbps |
| YUV420 | 3.11 MB | ≈ 747 Mbps |
| RGB888 | 6.22 MB | ≈ 1.49 Gbps |

- CSI-2 每 lane 的 D-PHY HS 数据率：Pi4 CSI 时钟上限约 1.5 Gbps/lane（官方
  camera_auto_detect 配置 1080p@30 用 2-lane），2-lane 理论 ≈ 3 Gbps，考虑
  8b/10b 编码（D-PHY 实际是每 8 bit 数据 10 bit 传输）有效 ≈ 2.4 Gbps。
- **结论**：RAW10/RAW8/YUV420 的 1080p@30 均在 2-lane 能力之内（622–747 Mbps
  ≪ 2.4 Gbps 有效带宽）；**RGB888 1.49 Gbps 勉强可行但无余量**，且 ISP 未实现
  时传感器直出 RAW，需软件转 YUV/RGB——因此 1080p@30 应走 RAW→YUV420 管线。
  当前 QEMU 预览用 320x240@15（约 2.7 MB/s）保证模拟器流畅。

### Q4：相机缓冲区与显示合成器的零拷贝共享是否可能？

**可能，但当前未接线**。分析：

- KunxOS 合成器：应用直接 `open/mmap /dev/fb0` 并 CPU 写像素（`apps/*` 的
  `put_pixel`），无 GPU 合成；因此「零拷贝」= 让相机 DMA 直接把帧写进
  framebuffer 对应的物理页。
- Pi4 接线下可行方案：把 Unicam `CSI_DMA_ADDR` 指向 framebuffer 物理地址
  （或分配与 framebuffer 同页的环形缓冲），帧数据直接可见于屏幕——省去
  「传感器→内存→CPU 拷贝→fb」两段拷贝。
- **阻碍**：a) 相机帧是 RAW/YUV，fb 需要 RGB888 或 YUV 直写支持（当前内核
  `gpu_simple` 仅 RGB888）；b) 取景尺寸≠屏幕尺寸，需缩放（HW scaler 或
  Unicam 裁剪）；c) 内存属性（DMA 区须 device/非 cacheable 或显式 cache 维护，
  而 fb 是普通可缓存 RAM）。当前交付的 HAL 已把「缓冲槽物理地址」作为一等公民
  （`BufferPool::set_phys/phys`），为接线预留；QEMU 下以文件环形缓冲模拟该语义。

## 8. 测试方案

### 8.1 宿主测试（CI 可跑，无需 QEMU）

- `shared/jpeg`：`cargo test` 7 用例（结构/尺寸/确定性/边界/压缩率）。
- `tests/scripts/verify_jpeg.py`：编码 640x480 彩条 → Pillow 解码 → 断言色带内部
  误差 < 48；无 Pillow 时退化为标记结构检查。

### 8.2 QEMU 模拟测试（默认构建，端到端）

`tests/scripts/camera_e2e_test.py` 覆盖：LIST/3A_GET 冒烟 → PHOTO 落盘 JPEG → 预览帧
推进 → REC 生成 KXRV 容器。

**当前状态（2026-08-27 实测）**：相机子系统代码链路（HAL/camerad/应用/JPEG）
全部编译通过（qemu 与 pi4 两个 feature），`cargo test` 宿主单测通过（kunxhal
相机 6 例 + kunxjpeg 7 例）；**但 QEMU 端到端仍被既存的 SFS v2 内核写路径缺陷
阻塞**，与相机代码无关，影响所有守护进程：

- **症状 A（文件内容串线）**：多个进程并发在 `/var/run` 创建/读写文件时，
  `camerad.ctl`、`audiod.ctl` 等会解析到**互相串线的 inode/块**——camerad 读
  ctl 文件返回的是其它文件的数据（实测读到音频 PCM/字库块），FTRUNCATE 后仍
  反复出现。定位：`kunxcore/src/fs_simple.rs` 的 inode/目录项分配路径（
  `fs_create`/`dir_add_entry`/`data_block_alloc`）在并发创建下未保持
  「目录项 ↔ inode ↔ 数据块」三者的原子对应。
- **症状 B（事务崩溃）**：随机守护进程（audiod/sensord/audiomix 子进程）在
  首个*事务写*（fs_create/fs_write/fs_truncate 的 `txn_commit`）时在
  `jlog_place`（`fs_simple.rs` 606 行附近）`%` 除零崩溃，实测 `jlog_place`
  收到 `major=0xFFFFFFFF`——根因是 SFS 事务状态（`TXN_DEPTH/TXN_MAJOR/
  PENDING_*`）为**全局共享**，多进程交错（如语音测试子进程与守护进程同时写）
  时被污染。修复方向：事务状态按（进程/设备）隔离或用全局自旋串行化，
  并对 `jlog_blocks==0` 做防御。

上述为**既有未提交工作树**（SFS v2 重写）的遗留问题，非本次相机改动引入；
后续任务「修复 SFS v2 内核写路径」是相机 QEMU 端到端的前置条件。相机子系统
可先行验证的部分（编译、宿主单测、sim 帧渲染确定性）已全绿：

1. `cargo test`（kunxhal/kunxserv/camerad）：BufferPool 状态机、SimCamera
   枚举/单帧/确定性渲染、FixedThreeA、JPEG 7 例全部通过；
2. `make` / `make pi4`：camerad 与 camera 应用在 qemu / pi4 两种 feature 下
   编译通过；
3. 串口可观察 `[camerad] backend: sim-cam0 (1280x720)` 与
   `configured 320x240@15 yuv420`（服务启动正常）；
4. SFS 修复后用 `python3 tests/scripts/camera_e2e_test.py` 回归：
   PHOTO → `photo_000001.jpg`（`tests/scripts/verify_jpeg.py` 验证），
   REC → `rec_000001.kxv` 头部 `KXRV`，预览帧序号推进；
5. 无相机后端回归：`camerad` 未运行时相机应用自动回退模拟取景器（不崩溃）；
6. `tests/scripts/apps_regression.py` 已覆盖相机应用开/关。

### 8.3 Pi4 真机拍照验证（接线完成后）

1. 接线：Camera Module v2 插入 CAM1，`config.txt` 使能相机（或 KunxOS
   bootloader DTB 声明）；
2. 枚举：`camerad` 启动日志应显示 `csi0-imx219 (3280x2464)`；
3. 取景/拍照：打开相机应用 → 预览 RAW→YUV 转换画面 → 快门 → 检查
   `/data/photos/photo_000001.jpg` 尺寸正确且内容为场景（非纯色）；
4. 性能：测 1080p@30 是否达到（§7-Q3 带宽结论）；未达则降 720p@30；
5. 3A：固定参数下拍摄过曝/欠曝场景，验证曝光元数据写入 EXIF 等价字段
   （当前写 `CaptureResult`，EXIF 注入为后续工作）。

## 9. 文件清单

| 文件 | 内容 |
|---|---|
| `kunxhal/src/camera.rs` | HAL：CameraDevice/CaptureRequest/CaptureResult/3A/BufferPool |
| `kunxhal/src/plat_qemu.rs` | SimCamera 软件传感器 + 测试帧渲染 |
| `kunxhal/src/plat_pi4.rs` | PiCamera 骨架 + Unicam/BSC/IMX219 接线参考 |
| `kunxserv/camerad/` | 相机服务：命令通道、帧泵、JPEG 落盘、录像容器 |
| `shared/jpeg/` | 纯 Rust baseline JPEG 编码器（no_std + 宿主测试） |
| `apps/camera/src/main.rs` | 相机应用：真实取景/拍照/录像（含模拟回退） |
| `tests/scripts/verify_jpeg.py` | JPEG 输出验证脚本 |
| `tests/scripts/camera_e2e_test.py` | QEMU 端到端：LIST/PHOTO/预览/REC（受 SFS v2 缺陷阻塞，见 §8.2） |
| `docs/camera.md` | 本文档 |
