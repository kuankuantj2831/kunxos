# KunxOS 动画系统规范（对标 iOS 与 Android）

> 目标：让 KunxOS 的界面动效全面对齐 iOS（Apple HIG / Core Animation）与
> Android（Material Motion）的运动语言 —— 相同的缓动曲线、相同的弹簧物理、
> 相同的转场节奏，跑在自研软件合成器与无堆 EL0 应用之上。

本文档是系统运动规范的权威来源。所有实现（内核动画引擎、launcher 运动库）
都必须与此处的数值一致。

## 1. 缓动曲线

三次贝塞尔时序函数 `cubic-bezier(x1, y1, x2, y2)`，输入进度 `x ∈ [0,1]`，
输出曲线值 `y ∈ [0,1]`。求解算法：先在 x 曲线（控制点 `0, x1, x2, 1`）上
用牛顿迭代 + 二分兜底求参数 `t`，再代入 y 曲线（控制点 `0, y1, y2, 1`）。

| 变体 | 控制点 | 平台来源 | 典型用途 |
| --- | --- | --- | --- |
| `Linear` | (0, 0, 1, 1) | 通用 | 匀速滚动、进度条 |
| `EaseIn` | (0.42, 0, 1, 1) | iOS `kCAMediaTimingFunctionEaseIn` | 快速进入 |
| `EaseOut` | (0, 0, 0.58, 1) | iOS `kCAMediaTimingFunctionEaseOut` | 按钮反馈、退出 |
| `EaseInEaseOut` | (0.42, 0, 0.58, 1) | iOS `kCAMediaTimingFunctionEaseInEaseOut` | 转场、缩放 |
| `FastOutSlowIn` | (0.4, 0, 0.2, 1) | Android `@android:interpolator/fast_out_slow_in`（Standard） | 标准进出场、按压 |
| `LinearOutSlowIn` | (0, 0, 0.2, 1) | Android `linear_out_slow_in`（Decelerate） | 元素进入 |
| `FastOutLinearIn` | (0.4, 0, 1, 1) | Android `fast_out_linear_in`（Accelerate） | 元素退出 |
| `Emphasized` | (0.2, 0, 0, 1) | Material 3 `Emphasized` | 强调转场 |

### 参考采样值（自检断言 ±0.01）

| 曲线 | `x = 0.5` 处 `y` |
| --- | --- |
| `Linear` | 0.5000 |
| `EaseIn` | 0.3152 |
| `EaseOut` | 0.6848 |
| `EaseInEaseOut` | 0.4998 |
| `FastOutSlowIn` | 0.7754 |
| `LinearOutSlowIn` | 0.8392 |
| `FastOutLinearIn` | 0.3248 |
| `Emphasized` | 0.8778 |

## 2. 弹簧物理

阻尼谐振子：`m·x″ + c·x′ + k·(x − target) = 0`。实现为半隐式欧拉积分
（1ms 子步），无 `exp`/`sin` 闭式解，纯四则运算，`no_std` 可编译。

| 风格 | 参数 | 平台来源 |
| --- | --- | --- |
| iOS | `dampingRatio`（0~1 欠阻尼 / 1 临界 / >1 过阻尼）+ `initialVelocity`（值/秒）；内部刚度取 `k = 280 N/m`、质量 `m = 1` | `UIView.animate(withDuration:delay:usingSpringWithDamping:initialSpringVelocity:)` |
| Android | `stiffness`（N/m，默认 210）+ `dampingRatio`（默认 1） | AndroidX `SpringAnimation` |

稳定性判据：`|x − target| < 0.0005` 且 `|v| < 0.001`（值单位）。

## 3. 交互动效规格

### 3.1 图标按压反馈（launcher）

| 阶段 | 动画 | 曲线 / 物理 | 时长 |
| --- | --- | --- | --- |
| 按下 | 图标 1.0 → 0.95 | `FastOutSlowIn` | 80ms |
| 松开（未启动） | 0.95 → 1.0 | iOS 弹簧 `dampingRatio = 0.65` | ~150ms |

### 3.2 应用启动转场（ColorOS app-open 风格）

点击图标后立即 `fork + execve` 并行加载应用，同时图标瓦片以
spring(300ms, bounce≈0.07) 从图标原位放大到覆盖全屏，动画后段白闪
淡入至不透明，衔接应用首帧（ColorOS 15 应用开启节奏，比 iOS
spring(0.35, bounce 0.1) 更利落，参数以 `Spring::launch()` 为准）。

| 属性 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 缩放 | 图标原位 → 覆盖全屏 | `Spring::launch()`（ζ=0.65） | 300ms |
| 白闪 | 0 → 255 | 按进度后段线性 | 300ms |

### 3.3 应用返回转场（ColorOS app-dismiss 风格）

应用退出后，以其最后一帧为快照，居中从全屏缩回图标原位，白色覆盖层
同步淡入（ease-out），随后重绘桌面。

| 属性 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 缩放 | 全屏 → 图标足迹 | `EaseOut` | 300ms |
| 白闪 | 0 → 255 | `EaseOut` | 300ms |

### 3.4 打断动画（ColorOS 取消启动）

启动转场未播完时上划取消：图标从中间撕成上下两半，各向对侧分离并
随撕裂进度向白色淡出（撕裂间距随滑动速度增大，10..30px），合计约
1s 后两半以弹簧（ζ=0.5）飞回原位重新合拢。

| 阶段 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 撕裂飞出 | 两半分离 + 白淡出 | `FastOutSlowIn` | 0.45s |
| 悬停保持 | 分离最大 | 线性 | 0.55s |
| 合拢飞回 | 两半弹簧合拢 | spring(ζ=0.5) | ~0.5s |

## 3.5 OOBE 向导动画（ColorOS 15 设置向导风格）

首次开机向导 13 步流程不变，动效对标 ColorOS 15 设置向导：

| 场景 | 动画 | 曲线 / 物理 | 时长 |
| --- | --- | --- | --- |
| 欢迎页徽标 | 0.5 → 1.0 弹跳放大（中段轻微过冲）+ 淡入 + 柔光晕开 | `ease_out_back` | ~80ms |
| 欢迎页文字 | 标题先行、副标题/标语错峰上滑淡入（视差位移 16/12/10px） | `EaseOut` | ~120ms |
| 步骤转场 | 旧页交叉淡化、新页上滑回弹入位（26px） | `ease_step` + `ease_out_back` | ~50ms |
| 选中卡片 | 卡片内强调色高亮脉冲淡出 | 线性 | ~110ms |
| 进度点 | 当前点放大 + 已完成点亮（ColorOS 绿） | 线性 | 随转场 |
| 进入系统 | 壁纸从白屏淡入（先快后缓） | `EaseOut` | ~360ms |

强调色统一为 OPPO/ColorOS 绿（`C_ACCENT = 0x00B96B`）。

### 3.6 合成器窗口动画（内核）与帧时钟

`kunxcore/src/anim.rs` 的动画管理器可对合成器窗口的
位置（X/Y）、透明度（Alpha）、缩放（Scale，定点 1.0 = 256）做
时间型补间或弹簧动画，由 `anim::frame_tick` 逐帧推进，配合
`compositor::composite + present` 形成动画渲染管线。该路径供
窗口系统（如拖动回弹、窗口开合缩放）使用。

**帧时钟（CNTP 物理定时器，120Hz）**：动画帧推进与调度器 tick 解耦 ——
调度 tick 走虚拟定时器 CNTV（IRQ 27，10ms 动态周期），动画帧推进走
非安全 EL1 物理定时器 CNTP（IRQ 30，**固定 8.33ms = 120Hz**，
`timer::timer2_set_interval(8_333)`，启动时注册到 `anim::frame_tick`）。
两条硬件通道互不干扰，帧间隔不再被调度器上下文切换抖动污染，也不受
100Hz 上限限制。

**应用直绘路径（脏区域合成器）**：桌面应用不再整屏 memcpy 到单缓冲，
而是 mmap `/dev/fb0`（= 内核持久画布 canvas，固定物理页、不随翻转旋转）
直绘，`app_kit::mark_dirty` 标记变更矩形，`FBIO_PRESENT` 触发
`compositor::fb_present` —— 内核只把「本帧 ∪ 上一帧」脏区从 canvas
拷贝到 back（拷贝量 = 脏区，非整屏），混合覆盖窗口后页翻转（双缓冲，
无撕裂）。应用直绘接管显示后（`compositor::fb_active`），
`anim::frame_tick` 只推进动画并标记脏区，**不再自行翻转**，避免在应用
绘制中途翻转造成撕裂。launcher 内的运动库实现上述 3.1~3.4（含打断撕裂）
与 3.7~3.10（翻页 / 最近任务 / 锁屏转场 / Toast）。

### 3.7 桌面翻页动画（launcher，ColorOS 15 风格）

跟手翻页 + 弹簧吸附，状态在 `apps/launcher/src/main.rs`
（`PAGE_OFFSET` / `PAGE_SPRING`，弹簧 `Spring::page()`）：

| 阶段 | 动画 | 曲线 / 物理 | 时长 |
| --- | --- | --- | --- |
| 跟手 | 页面随手指平移，页界外 0.35x 橡皮筋阻尼 | 线性（1:1 跟手） | 实时 |
| 翻页吸附 | 新当前页从 ±屏宽 弹回 0（内容连续滑动） | `Spring::page()`（ζ=0.8，≈260ms，近临界无过冲） | ~260ms |
| 回弹 | 原页弹回 0 | 同上，初速继承手势速度（±4000px/s 钳制） | ~260ms |
| 页码点 | 整体随进度平移 1 格，当前页/目标页颜色交叉过渡 | 线性 | 随进度 |

判定：拖动 >36px 且横向占优进入翻页；松手位移 >1/4 屏 或速度
>600px/s 翻页。绘制上当前页与相邻页按 `draw_page_at(page, offset)`
平移（壁纸/状态栏/Dock 不动），页界外整项跳过。渐进式启动
（`BOOT_ACTIVE`）期间保持瞬切（图标缓存未按需构建）。

### 3.8 最近任务面板动效（launcher，ColorOS 15 风格）

| 场景 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 打开入场 | 卡片按槽位错峰 30ms，上滑 24px 入位 | `EaseOut` | 200ms/卡 |
| 关闭 | 整体下滑 12px + 全屏压暗同步淡出 | `EaseOut` | 150ms |
| 单卡清理跟手 | 卡片随手指垂直位移 | 线性（1:1） | 实时 |
| 单卡清理上飞 | 上飞 360px 出屏（已从列表移除） | `FastOutLinearIn` | 200ms |
| 清理回弹 | 未达阈值（-80px）的卡片弹回原位 | `EaseOut` | 200ms |
| 其余卡片补位 | 从旧格位到新格位 x/y 同步插值 | `EaseOut` | 220ms |

实现：`draw_recents` + `recents_advance`（主循环逐帧推进）；
后台应用返回动画结束（`return_animation_done`）与应用退出码 2
均经 `recents_open()` 入场。

### 3.9 锁屏解锁 / 锁定转场（launcher，ColorOS 15 风格）

锁屏整层（渐变底 + 时钟/日期/键盘/图案内容）作为一个层垂直平移：
解锁时上滑出屏露出桌面，锁定时从屏幕上方下滑入位。层偏移由
`LOCK_LAYER_OFF` 驱动，`render_frame` 在转场期间先绘桌面再叠锁屏层。

| 场景 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 解锁转场 | 锁屏层从当前位置上滑一屏（含 PIN/图案解锁成功） | `FastOutSlowIn` | 300ms |
| 锁定转场 | 锁屏层从 -屏高 下滑入位 | `FastOutSlowIn` | 320ms |
| 滑动解锁跟手 | 层随手指 1:1 上移（下拖 0.15x 阻尼） | 线性 | 实时 |
| 跟手结算 | 位移 >18% 屏高或上滑速度 >600px/s → 解锁，否则回弹 | `EaseOut` | 250ms |

转场期间锁屏输入全部屏蔽（`handle_lock_input` 对 `LOCK_PHASE != None`
直接消费）；启动/远程锁定（`/data/oobe/locked=1`）经 `lock_now()`
播放锁定转场，开机初始 `LOCKED` 直读（无转场）。

### 3.10 Toast 浮层动效（launcher）

`draw_overlay` 渲染期间临时降低全局像素透明度 `PIX_ALPHA`
（`put_pixel` 对 BACK_BUF 按 alpha 混合，恢复 255 后零开销），
玻璃药丸与文字整体淡入淡出：

| 阶段 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 入场 | 底部上滑 16px + 淡入 | `FastOutSlowIn` | 180ms |
| 保持 | 完全不透明 | — | ~1.67s |
| 退场 | 淡出 | 线性 | 150ms |

### 3.11 共享运动库（shared/motion.rs）

应用侧动画的公共底座（no_std、无堆、纯四则运算，经 `#[path]` 引入，
同 nav.rs 模式）：8 种贝塞尔缓动（牛顿迭代 + 二分兜底，与内核
`kunxcore/src/anim.rs` 同算法）、阻尼弹簧（半隐式欧拉 1ms 子步、数值稳定
防护）、`Spring::retarget(target, pos, vel)` 打断接续（旧动画当前值/速度
无缝交给新目标，鸿蒙 springMotion 衔接语义的应用侧等价物）、
`progress` / `triangle`（无 sin 三角波呼吸）/ `lerp`。

### 3.12 壁纸切换交叉淡化（launcher，ColorOS 换壁纸过渡）

应用返回前台（返回转场结束 `return_animation_done`）时重读桌面偏好；
壁纸/主题调色板变化即触发。调色板 = (顶部渐变、底部渐变、棋盘强调色)。

| 属性 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 调色板混合 | 旧 → 新（行级混合，逐像素零额外开销） | `EaseOut` | 300ms |

### 3.13 锁屏充电动效（launcher，ColorOS 幻光影流动风）

充电中（`/data/oobe/charging`，1s 缓存轮询）锁屏显示电池药丸 +
百分比，位于 PIN/图案输入区下方空档（cy=620），随锁屏层平移：

| 阶段 | 动画 | 物理 | 参数 |
| --- | --- | --- | --- |
| 双圈涟漪 | 圆环扩散淡出（整数中点圆 alpha 混合） | 线性 | 周期 1.6s，圈间滞后 0.8s，半径 18→64px，alpha 120→0 |
| 呼吸光晕 | 药丸外三环 alpha 振荡 | 三角波 0..1..0 | 周期 1.2s，基础 alpha 70/46/24 × (0.55 + 0.45·波值) |
| 电量填充 | 药丸内宽度 = 电量比例 | — | 实时 |

### 3.14 控制中心滑条数值气泡（launcher）

拖动亮度/音量滑条时，被拖滑条上方居中弹出玻璃药丸（68×30，圆角 15）
实时显示百分比；数值变化跟随拖动，松手后保持 900ms 消失
（`cc_advance` 在此期间保持重绘）。

### 3.15 应用页面切换转场（settings，鸿蒙页面转场规范）

页面栈推入/弹出（`page_push` / `page_pop`）240ms `FastOutSlowIn` 左右
位移转场。实现：离场页整帧快照至 `SNAP_BUF`（2MB），每帧渲染新页后
行级平移合成——push 为新页自右滑入、快照补左；pop 为父页左移入位、
快照从右侧滑出——接缝处 10px 线性衰减阴影（最强 alpha 90）投影在被
覆盖页上。转场期同步推进循环（阻塞 ~240ms，输入由内核输入栈缓冲，
8ms 帧预算）。

### 3.16 控制中心 / 通知中心跟手下拉（launcher，ColorOS 15 / 鸿蒙 面板跟手语义）

顶部区域按下并纵向拖动时面板进度 1:1 跟随手指，替代原先"须一次性
flick >80px 才瞬发弹簧"的开合方式。区域与 flick 路径一致：左上
（x<120）= 通知中心，右上（x≥W-50）= 控制中心。状态机在
`apps/launcher/src/control_center.rs`（`CcPhase::Drag` +
`shade_drag_*`），拖动管线接入 `apps/launcher/src/main.rs` 2.45 跟手段。

| 阶段 | 动画 | 曲线 / 物理 | 时长 |
| --- | --- | --- | --- |
| 跟手 | 面板进度 = dy/80px（1:1，钳制 0..1），弹簧停摆 | 线性 | 实时 |
| 速度估计 | 手指速度低通（0.6 旧 + 0.4 新），供 fling 判定 | — | 实时 |
| 结算打开 | 弹簧自当前进度起跳并继承手指速度（进度/秒） | spring(k=420, ζ=0.85) | ~250ms |
| 结算回落 | 从当前进度回落（不闪跳全开） | `EaseOut` | 200ms |
| 通知中心续接 | 200ms EaseOut 时间轴按当前进度偏移续播 | `EaseOut` | 剩余时长 |

判定：进度 >30% 或速度 >600px/s 打开，否则回落。结算后吞掉抬手产生
的一次手势，与 flick 开合路径互斥不双触发；跟手期间翻页语义被
`page_drag_allowed` 屏蔽。与鸿蒙 springMotion 打断接续同构：面板永远
从「当前的位置与速度」继续运动，而非回跳到动画起点。

### 3.17 最近任务卡片启动锚点（launcher，ColorOS 15 / 鸿蒙 卡片起点语义）

从最近任务卡片点开应用时，启动/恢复放大转场以**所点卡片中心**为起点
（原为桌面图标原位）。一次性锚点 `LAUNCH_ANCHOR`（main.rs
`set_launch_anchor` / `take_launch_anchor`）由 `start_launch` 与
`resume_app_at`（app_life.rs）消费：新启动从卡片中心弹簧放大覆盖全屏，
后台恢复的快照同样从卡片中心放大。返回转场仍缩回图标原位（ColorOS
同款：回到桌面图标语义位置）。

### 3.18 通话横幅 Fluid-Cloud 入退场（launcher，ColorOS 15 流体云胶囊）

桌面态来电/通话横幅（rild.state 轮询）出现/消失不再瞬现瞬消：

| 阶段 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 入场 | 从状态栏后方弹入（下滑 46px）+ 整体淡入 | `FastOutSlowIn` | 260ms |
| 退场 | 上滑收回状态栏后方 + 整体淡出 | `EaseIn` | 220ms |

实现：`draw_call_banner` / `draw_banner_at`（PIX_ALPHA 整体透明度，同
Toast 机制）/ `banner_anim_active`（主循环动画期保持重绘）；退场期间
`call_banner_hit` 不可点击。文本拼装收敛到 `call_banner_layout`，绘制
与命中测试共用同一矩形。

