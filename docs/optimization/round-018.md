# 优化轮 018：上滑退出应用「卡一下」根治——返回转场桌面预烘焙，手势路径 550ms→24ms

- 日期：2026-10-18
- 轮次性质：用户直接指令轮（「全面屏手势上滑退出应用时总会卡一下」）
- 类型：交互性能（launcher 返回转场关键路径）

## 症状与根因

全面屏手势上滑退出应用时，应用末帧先冻结明显一拍（「卡一下」）再播返回动画。

链路：应用 `lc_home()` → lifd 转发 BG → launcher 前台轮询（4ms 内检出）→
`snapshot_frame()`（fb0→APP_SNAP 2MB）→ `start_return()`：
fb0→FRAME_BUF 2MB + `wallpaper_sync_from_prefs()` + **`draw_desktop_full()`
整屏桌面渲染** + BACK_BUF→DESK_BUF 2MB → 才允许首帧动画。

根因：`draw_desktop_full()` 在**手势关键路径上同步执行**——壁纸 + 状态栏 +
21 个图标的 TTF 文字 + 玻璃 dock 全部重画，TCG 实测 **550ms**。而这段渲染
完全是冗余的：launcher 在应用前台期间持续运行但从不渲染桌面，桌面状态
在启动转场结束时已定格，返回时刻的桌面与之完全一致（启动时若预渲染过
DESK_BUF 则更是现成）。

## 修复：应用前台空闲期预烘焙

| 文件 | 改动 |
| --- | --- |
| `app_launch.rs` | 新增 `desk_bake_for_return()`：主循环前台分支在**应用首帧已上屏（占位页停）且 MOTION==Idle** 时调用一次——把整幅桌面渲染进 DESK_BUF（不 flip、不触碰显存，应用独占屏幕期间完全隐形）。`DESK_BAKED` 会话级防重入，`start_launch` 复位。新增 `placeholder_on()` 访问器 |
| `app_launch.rs` | `start_return()` 改条件渲染：仅 `!DESK_PRERENDERED \|\| 壁纸变化`（`wallpaper_sync_from_prefs` 的调色板比较）才兜底整屏渲染；烘焙命中时手势路径只剩 2×2MB 拷贝 + 状态设置。固化两行诊断打印（预烘焙耗时 / 上滑返回就绪耗时 / 兜底渲染命中） |
| `main.rs` | 前台分支接线：`!in_resuming() && !placeholder_on() → desk_bake_for_return()` |

安全边界：烘焙只在 MOTION==Idle（无任何动画使用 BACK_BUF 作画布）时执行；
桌面上可能过期的状态（状态栏分钟数字、呼入/健康横幅）由动画结束后的常规
整屏重绘刷新（≤300ms 后），与分钟级横幅变化相比不可感知；壁纸在设置类
应用中修改的场景走兜底重渲染路径（WP_FADE_ACTIVE 调色板比较检出）。

## 验证（QEMU TCG 实测，串口计时打印）

| 场景 | 预烘焙（空闲期，隐形） | 手势路径就绪 | 返回动画 |
| --- | --- | --- | --- |
| weather（网格图标启动） | 550ms | **24ms** | 11 帧 |
| settings（dock 启动） | 526ms | **21ms** | 15 帧 |

修复前手势路径 ≈ 同样的 500ms+ 整屏渲染同步执行 → **~23x**。两轮均烘焙
命中（无兜底渲染打印）、0 PANIC、回桌面画面像素统计正常（140 色，暖色
壁纸基调）。

## 遗留问题 / 下一轮建议

1. OOBE 转场的「导航区」相位仍 49-123ms/帧（自动化轮 15-17 的 A/B 在途，
   归属其域）。
2. `snapshot_frame` 与 FRAME_BUF 双 2MB 拷贝可在未来合并为单次（收益 ~2ms，
   已非瓶颈）。
3. 真机上 550ms 渲染会显著小于 TCG，但预烘焙方案两平台通用（都改为零
   手势路径渲染）。
