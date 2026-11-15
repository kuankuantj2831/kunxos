# KunxOS 星光与毛玻璃

视觉方向：珍珠白、雾蓝和淡紫组成安静的星光背景；少量四角星芒提供识别度；磨砂半透明面板承载内容，深灰文字保证阅读。交互强调色为 `#5B63D9`，危险与错误仍使用红色。

## 实现

- `shared/starlight.rs`：无堆程序化背景、柔和光域和固定星点；不依赖位图下载，不持续播放粒子动画。区域绘制与整屏绘制结果一致。
- `shared/glass.rs`：沿用背景降采样、模糊、双线性还原和圆角裁剪；降低白色着色不透明度及镜面高光，增加背景透出程度，统一蓝紫色主操作材质。
- 桌面与锁屏使用同一壁纸缓存；四套壁纸保留不同的冷色变化。桌面局部刷新继续复用缓存；锁屏滑动复制对应背景行。
- 首次开机向导缓存完整星光背景，未选中选项改用实际磨砂玻璃，进度和选中态统一蓝紫色。
- 设置和使用 `app_kit::draw_gradient_bg` 的应用采用共享星光背景；`ds` 与公共应用强调色同步。
- 控制中心降低白色覆盖，透出下方壁纸的冷色层次。

布局、应用启动入口与输入命中区域沿用现有实现。独立绘制的应用内容（例如相机画面、终端正文）保留其用途所需的颜色。

## 验证入口

```powershell
rustc --edition=2021 --test shared/starlight.rs -o build/starlight-tests.exe
& build/starlight-tests.exe
python tools/test_display.py --qemu 'C:/Program Files/qemu/qemu-system-riscv64.exe' --build-dir build/windows --animate --resize --host-cursor --all-apps
python tools/test_display.py --qemu 'C:/Program Files/qemu/qemu-system-riscv64.exe' --build-dir build/starlight-oobe --oobe --resize
```

默认 Windows 启动入口读取 `build/windows/target/riscv64gc-unknown-none-elf/release/rust-kernel`。构建及验证完成后，双击根目录的 `启动KunxOS.bat` 即可查看更新的界面。

2026-11-15 验证：星光区域刷新与边界裁剪的两项宿主测试通过；相关应用 RISC-V release 构建通过；QEMU 桌面及向导的显示缩放验证通过，桌面另覆盖设置/电话打开与返回、宿主指针及显示表面保持检查。报告与实际截图分别保存在 `build/windows/` 和 `build/starlight-oobe/`。这些结果不代表真机帧率或硬件垂直同步已验证。

已知限制：最终镜像第一次复测在设置空白处点击一致性检查中失败，截图对比显示点击前已经残留大面积绿色启动占位页；同一镜像随后完整复测通过。残留原因尚未确认，不能把一次通过视为启动交接问题已彻底解决。失败日志保存在 `build/starlight-final-run.log`，随后通过的日志为 `build/starlight-final-rerun.log`。

## 大窗口启动与桌面重启修复

用户故障截图对应的运行进程使用 `build/oobe-preview/target/riscv64gc-unknown-none-elf/release/rust-kernel` 旧镜像。该次串口日志记录了 `1920x1001` 下 launcher 在 `src/main.rs:3075` 重复 panic，最终触发 init 的重启上限。

根因是 launcher 只在运行期分辨率改变时调整 `PixelBuffer` 的可见长度；首次映射 framebuffer 后仍保留 `540*960` 的默认长度。OOBE 在宽窗口中结束或桌面在宽窗口中重启时，首次整屏绘制会越界。现在首次映射与动态缩放共用 `resize_buffers()`，同步背景、桌面、状态栏、Dock、转场和应用快照长度，并初始化系统导航尺寸、控制中心缓冲；初始尺寸也按共享显示容量验证。

新增回归入口：`tools/test_display.py --no-build --build-dir build/windows --resize --host-cursor --restart-launcher`，在 1920×1001 与 540×960 下分别终止 launcher，让 init 重新启动它，检查新实例映射尺寸、整屏渲染完成以及无 panic。

此回归已通过，报告包含 `launcher_restart_modes: [[1920,1001],[540,960]]`。修复已打入默认 Windows 镜像。用户界面的旧预览进程更换为默认镜像；原崩溃日志另存于 `build/starlight-native/old-preview-crash.log`。回归日志为 `build/launcher-cold-start-run.log`。
