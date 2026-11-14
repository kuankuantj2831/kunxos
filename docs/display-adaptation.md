# 显示适配与流畅度

Windows 使用原生 QEMU SDL 窗口，启动器观察该进程的客户区尺寸，通过 localhost
串口把尺寸送进来宾。桌面和已迁移应用按新尺寸重新绘制，文字不会作为旧截图被拉伸。
当前支持 360–1920 × 480–1200 像素，默认 540×960；范围以外的窗口请求会被钳制。

## Windows 启动

```powershell
powershell -File tools/run-windows.ps1
# 已构建后快速打开
powershell -File tools/run-windows.ps1 -NoBuild
# 指定新构建的 ELF 或 kernel.img；自动检测宿主刷新率，也可显式指定目标
python tools/run_windows.py --kernel path/to/rust-kernel --rate 144
```

需要 Windows 上的 Python 和 QEMU。默认 QEMU 路径为
`C:\Program Files\qemu\qemu-system-riscv64.exe`，可用 `--qemu` 改写。
优先使用 `build/windows/target/riscv64gc-unknown-none-elf/release/rust-kernel`，
没有该构建时回退到 `kunxcore/kernel.img`。旧镜像必须重新构建才能包含下面的协议；
启动器不能给旧镜像补上驱动。关闭 QEMU 窗口后，桥接进程随之退出。
串口日志默认保存在项目目录。SDL 使用 per-monitor-v2 DPI 模式，避免 Windows
位图拉伸；桥接器采用与 QEMU 一致的 DPI 坐标，避免 125% 缩放导致尺寸反馈循环。

启动器传入 `display-resize host-cursor display-hz=<Hz>`。窗口拖动事件合并 60ms 后发送，
来宾在两帧之间提交尺寸。窗口最小化时不改变分辨率。
不启用 `display-resize` 的旧启动命令仍使用原来的一屏显存分配。

## 已接通的路径

- ramfb 的扫描尺寸、stride、合成器脏区和输入坐标随模式更新，显示 generation 递增。
- 开启桥接时预留三块最大容量的扫描/画布存储，物理页在启动时确定。切换尺寸不替换
  canvas 地址，已有 framebuffer mmap 在进程生命周期内有效，不存在旧页被释放后继续写入。
- 用户态全屏缓存改成按需分页的 PixelBuffer。当前有效 slice 长度随尺寸变化，
  快照复制不读取未使用尾部；虚拟容量固定，不把所有应用的最大屏幕缓存塞进 ELF BSS。
- 桌面重排网格列数/行数，更新 Dock、状态栏、导航、控制中心、锁屏和转场几何，
  使依赖尺寸的壁纸/玻璃缓存失效。设置保留当前页、滚动位置与输入内容。
- 17 个使用 app_kit 的应用，以及设置、计算器、终端、商店、OOBE 已接入动态尺寸。
  每个页面在所有窄屏和短屏上的可用性仍需继续逐项验收，编译通过不等于布局验收。
- 真机 HAL 可传入屏幕密度、stride、实际刷新率；Pi4 保留固件 pitch。
  目前设备原生模式枚举、热插拔、旋转和硬件 VSYNC 仍需要对应平台驱动实现。
  新 QEMU 桥接不是通用真机 modeset 驱动，高密度字体全面迁移也尚未完成。

## 刷新节奏与动画

桌面与 OOBE 使用单调时钟、绝对帧期限，绘制耗时从等待时间扣除，错过的槽位直接跳过。
输入提前唤醒不会把下一帧期限不断往后推。目标节奏包含宿主提示/设备能力和电源限制。
桌面快照动画优先双线性逐像素采样，持续超预算才降低采样精度，有余量后逐级恢复；
1:1 快照直接复制。

修正 ELF 新页的重复查找，以及 exec/exit 释放映射记录时的平方级去重扫描。
去重仍保留最早记录的 ownership/COW 标志，OOM 时回退原来的无分配算法。
ELF 相邻段共享同一页的加载语义保持不变。

`display` 命令报告实际刷新率与软件目标，`perf stats` 报告呈现统计。
**ramfb 没有硬件 VSYNC；目标 144Hz 不代表实际达到 144FPS。**
Windows 启动时检测的刷新率是宿主提示，未知的设备实际刷新率仍为 0。

## ABI

`/dev/fb0` ioctl `0x4604` 返回 40 字节小端数据：

| 偏移 | u32 字段 |
| --- | --- |
| 0 | version = 1 |
| 4 | generation |
| 8, 12 | width, height |
| 16 | stride_bytes |
| 20 | density，1024 = 1x |
| 24 | refresh_millihz，0 = 未知 |
| 28 | target_millihz |
| 32 | capabilities，bit 0 = VSYNC，bit 1 = resize，bit 2 = host cursor |
| 36 | reserved = 0 |

`0x4605` 无参数：协作应用在帧边界提交最新待处理尺寸，随后重新查询显示信息。
忙于呈现时返回 EBUSY，应用稍后重试。前台应用提交切换，桌面在后台只同步已提交的几何。
串口扩展帧为 `AA 03 width_lo width_hi height_lo height_hi`，仅在启用桥接时接受，
IRQ 解析器只保存最新请求，不在中断中分配或切换显存。

## 验证

```powershell
python tools/test_display.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --resize --animate --all-apps
python tools/test_display.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --oobe --resize --rate 60
```

脚本在临时目录构建独立 rootfs/内核，使用 `KUNX_RAMDISK` 指定内嵌镜像，
不覆盖工作目录的 rootfs、ramdisk 或 kernel.img。报告目录打印在命令输出中。
运行共享显示算法和页记录测试，验证共享 ELF 边界页、目标节奏与能力声明、
竖横屏切换和实际 PNG 输出尺寸，以及设置打开/返回动画。
`--all-apps` 重建所有已迁移应用，并验证共享绘图库应用的尺寸切换。
日志、截图和 JSON 报告留在测试目录。

尚需真机驱动、跨密度文字、所有应用全部页面、超出支持范围的窗口、快速交错操作、
旋转、热插拔、内存压力与真实 VSYNC 的独立验收。

## 鼠标与 ramfb 呈现

ramfb 写配置会让 QEMU 重建显示表面，SDL 随之重建纹理，不能用作每帧页翻转。
合成器现在保留固定的 front 扫描缓冲，在 back 完成合成后仅复制脏区到 front；
驱动缓存地址、尺寸与 stride，只在首次绑定或分辨率改变时写入 fw_cfg。
这是软件脏区提交，仍无硬件 VSYNC 保证。
`display` 的 `presents` 与 `reconfigurations` 计数可检查普通刷新是否意外重建表面。
空闲桌面的 Dock 悬停只重绘 Dock 带区，避免重绘整屏壁纸、文字和小组件。
验证脚本覆盖 1920×1001 下的连续鼠标输入、Dock 悬停与后续分辨率恢复，
并断言鼠标刷新增加呈现次数而不增加显示表面重建次数。

## 光标与应用启动

Windows SDL 使用 `show-cursor=on` 和 `host-cursor` 启动契约，由宿主独立显示指针；
协作应用通过显示能力 bit 2 停止绘制软件指针，因此点击、加载和切换应用不会覆盖指针。
其他启动方式保留软件光标。桌面软件光标在 flip 内擦除旧位置并重画新位置，
内容与指针一起提交；重复 overlay 调用不会再把指针自身保存为背景。

应用启用 `third_party/fontdue` 的可选 `lazy` 特性，按首次使用缓存不可变字形轮廓，
避免启动时解析所有字形。原始字体保留在 Font 中，原有度量、抗锯齿和字形缓存继续使用。
原版与惰性版本的字形度量、像素输出、克隆和并发首次读取由
`cargo test --release --manifest-path tests/font_startup_host/Cargo.toml` 对比验证。
内核仍使用上游 fontdue，未启用惰性解析。

启动快照复用已绘制桌面，放大瓦片预先插值、按行复用采样。动画时钟从第一幅可见帧开始，
限制被抢占后的单次追赶，避免加载耗时吞掉全部中间帧。快速退出时先清除前台归属再返回，
避免应用在启动交接中退出后，启动器永久停在前台轮询状态。
