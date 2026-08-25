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
| 按下 | 图标 1.0 → 0.85 | `FastOutSlowIn` | 120ms |
| 松开（未启动） | 0.85 → 1.0 | iOS 弹簧 `dampingRatio = 0.65` | ~150ms |

### 3.2 应用启动转场（iOS app-open 风格）

桌面以被点图标为中心放大至 1.12，同时白色覆盖层淡入至不透明，
转场完成后再 `fork + execve` 拉起应用。

| 属性 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 缩放 | 1.0 → 1.12 | `EaseInEaseOut` | 300ms |
| 白闪 | 0 → 255 | `EaseInEaseOut` | 300ms |

Android 等效做法：淡入转场（`Standard` 300ms，缩放 0.97→1.0）——
实现里两者共用同一转场框架，仅换曲线与参数即可切换平台风格。

### 3.3 应用返回转场（iOS app-dismiss 风格）

应用退出后，以其最后一帧为快照，居中从 1.12 缩回 1.0，白色覆盖层
同步淡出，随后重绘桌面。

| 属性 | 动画 | 曲线 | 时长 |
| --- | --- | --- | --- |
| 缩放 | 1.12 → 1.0 | `EaseInEaseOut` | 280ms |
| 白闪 | 255 → 0 | `EaseInEaseOut` | 280ms |

### 3.4 合成器窗口动画（内核）

`kunxcore/src/anim.rs` 的动画管理器可对合成器窗口的
位置（X/Y）、透明度（Alpha）、缩放（Scale，定点 1.0 = 256）做
时间型补间或弹簧动画，由定时器（10ms）驱动的 `frame_tick` 逐帧推进，
配合 `compositor::composite + present` 形成动画渲染管线。该路径供
窗口系统（如拖动回弹、窗口开合缩放）使用；当前桌面应用走 `/dev/fb0`
直绘路径，由 launcher 内的运动库实现上述 3.1~3.3。

## 4. 实现位置

| 模块 | 位置 | 内容 |
| --- | --- | --- |
| 内核动画引擎 | `kunxcore/src/anim.rs` | 曲线求解、弹簧积分、动画管理器、`frame_tick`、自检 |
| 合成器缩放 | `kunxcore/src/compositor.rs` | 窗口 `scale` 字段、最近邻缩放合成、`set_scale` |
| 定时器多回调 | `kunxcore/src/timer.rs` | 多 handler 支持（调度 tick + 动画帧 tick） |
| launcher 运动库 | `apps/launcher/src/main.rs` | 曲线/弹簧（同规范）、按压反馈、启动/返回转场 |
| 自检 | 内核启动 `self_test` | 曲线采样值与弹簧收敛断言 |

## 5. 扩展指南

给新界面加动效的固定步骤：

1. 选曲线：进场用 `FastOutSlowIn` / `LinearOutSlowIn`，出场用
   `FastOutLinearIn`，转场缩放/淡入淡出用 `EaseInEaseOut`，物理回弹用弹簧。
2. 定时长：按压 100~150ms，进场 200~300ms，转场 250~350ms，弹跳 < 500ms。
3. 内核窗口：`anim::tween(window, prop, from, to, dur_us, easing, on_finish)`
   或 `anim::spring(window, prop, from, to, spring, on_finish)`。
4. 用户态直绘：参考 launcher 的运动状态机（`Phase` + `motion_advance`），
   在主循环每帧推进并重绘。
