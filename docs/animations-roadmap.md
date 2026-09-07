# KunxOS 动画增强与改进路线（对标 ColorOS 15 与 HarmonyOS 7）

> 本文是 `docs/animations.md` 的增量路线图：盘点现状、对标 ColorOS 15
> （极光引擎 / 流体云 / 幻光影流动）与 HarmonyOS 7（一镜到底 / springMotion /
> 空间光效），给出「增加什么、改进什么」的优先级清单。数值与落点文件均已
> 按当前代码核实。

## 1. 现状盘点（已实现，无需重做）

| 层 | 已有能力 |
| --- | --- |
| 运动库 | 8 种贝塞尔缓动 + 阻尼弹簧（半隐式欧拉），launcher 与内核各一份 |
| 内核 | `kunxcore/src/anim.rs` tween/spring 驱动窗口 X/Y/Alpha/Scale；`gesture.rs` 返回手势与压暗层；页面切换交叉淡化；120Hz CNTP 帧时钟 |
| launcher | 启动/返回转场、打断撕裂、翻页跟手+弹簧、最近任务（错峰/跟手清理/补位）、锁屏转场、Toast、图标按压、触摸涟漪、编辑模式 2° 抖动、文件夹弹簧开合、控制中心（开合弹簧 250/200ms、磁贴按压、通知面板） |
| OOBE | 徽标弹跳、文字错峰、步骤转场、闲置呼吸（0.975..1.025）、常驻动画 |
| 材质 | `shared/glass.rs` 毛玻璃（降采样+盒式模糊+高光+圆角）、alpha 混合、垂直渐变 |
| 应用侧 | 16 个桌面应用（settings/store/files/gallery/…）**动画为零**，页面切换全部瞬切 |

## 2. 对标结论

- **ColorOS 15** 的核心是「极光引擎」：**并行动画 + 全场景可打断**（图标/卡片/
  小组件/文件夹连续启动打断），配合「缩略图转场」「流体云非线性动效」「幻光影
  流动（充电/指纹/AOD 连贯动效）」「实时混色模糊与弥散投影」。
- **HarmonyOS 7（API 26）** 的核心是「空间化设计」：**光随指动、光线勾勒、
  非线性形变**；应用框架层提供 **springMotion / responsiveSpringMotion 弹簧曲线**
  （默认 response 0.55 / dampingFraction 0.825 与 0.422 / 0.86）、
  **geometryTransition 共享元素转场（一镜到底）**、**overlapDuration 动画参数
  平滑接续**。
- 两家共同原则：**连续性**（转场元素不消失）、**物理感**（弹簧+速度继承）、
  **可打断可逆**、**跟手 1:1 + 越界阻尼**。

## 3. 增加项（按优先级）

> 状态（2026-09-05）：P0-1 / P0-2（settings）/ P0-3 / P0-4（滑条气泡形态）/
> P0-5 已实施，数值并入 `docs/animations.md` §3.11~3.15。其余待后续轮次。

### P0-1 应用侧共享运动库 `shared/motion.rs`（前置基础设施）【已实施】

16 个应用零动画的根因是没有可复用运动库（glass/nav 已走 `#[path]` 共享，
motion 没有同等物）。从 launcher 抽出 `Easing`/`Spring`/逐帧推进 helper，
应用经 `#[path = ".../motion.rs"]` 引入。附带 `FrameClock`：沿用
`apps/launcher/src/main.rs:6764` 的自适应帧预算（动画期间 8ms、静止 16ms）
与 OOBE「等待输入期间定时唤醒」的常驻动画机制。

### P0-2 应用页面切换转场（鸿蒙页面转场规范）

应用二级页进出（settings 子页、files 目录、store 详情、sms 会话）由瞬切改为
**新页自右位移 40px + 淡入 240ms `FastOutSlowIn`，旧页左移 20px + 淡出**；
返回反向。鸿蒙规范明确：层级转场用左右位移 + 弹簧曲线，禁止单帧切换。
逐应用迁移：settings → files → store → sms → gallery。

### P0-3 壁纸切换过渡

`apps/launcher/src/main.rs:1293`、`main.rs:1320` 的 `WALLPAPER = wallpaper`
为瞬切。加 **300ms 交叉淡化 + 轻微缩放 1.04 → 1.0（`EaseOut`）**；编辑模式
进出时壁纸同步 0.98 缩放 + 压暗 8%，形成 ColorOS 式层级感。

### P0-4 音量/亮度 HUD 动效

系统当前无独立音量 HUD。按音量键弹出玻璃药丸（复用 glass）：
**缩放 0.9→1.0 + 淡入 200ms `LinearOutSlowIn`，2s 后 150ms 淡出**，
音量变化时进度条长度弹簧跟随（`responsive` ζ≈0.86）。

### P0-5 充电动效（ColorOS 幻光影流动）

锁屏下插入充电器：屏幕下方**涟漪扩散圆环**（2 圈错峰，800ms `EaseOut`，
alpha 120→0）+ 电池图标**呼吸光晕**（1.2s 正弦 α 0.6..1.0，与 OOBE 闲置呼吸
同款实现）；电量整数变化时数字滚动过渡。关机充电画面复用同款涟漪。

### P0-6 列表/卡片入场错峰

settings 主列表、store 卡片、launcher 编辑模式图标：**每项错峰 25ms、
上滑 20px + 淡入 200ms `EaseOut`**（与最近任务入场 `recents_open` 同参数，
复制其推进逻辑即可）。仅页面首次进入播放，滚动不重播。

### P1-1 全场景可打断动画（ColorOS 极光引擎核心卖点）

现状：锁屏转场期间输入被直接消费（`handle_lock_input`）、控制中心
Opening/Closing 不可拖动接管、文件夹开合不可反向。改造为**状态 + 速度接续**：
- 文件夹 `FOLDER_ANIM`、控制中心 `CC_SPRING` 打断时以当前值/速度反向续跑；
- 锁屏解锁中途回拉 → 直接从当前偏移进入回弹或反向锁定（`LOCK_FROM` 已有
  起点 Mechanism，扩为双向）；
- 返回转场中途点图标 → 立即反向（撕裂动画已有先例，推广为通用模式）。

### P1-2 共享元素「一镜到底」（鸿蒙 geometryTransition）

选 3 个高频路径：**gallery 缩略图→大图**（最重要的门面）、store 应用卡→详情、
设置列表行→子页。实现：记录起点/终点矩形，转场期在顶层以
`Emphasized`（300ms）补间 x/y/w/h + 圆角 24→0，目标页内容在 150ms 后交叉
淡入。瓦片渲染（`ICON_TILE`/`FOLDER_TILE` 模式）已提供先例，复制该机制做
`SharedTile`。

### P1-3 流体云 / 灵动胶囊（ColorOS 流体云）

状态栏左侧药丸容器：音乐播放/计时器/充电时收缩为 84px 药丸，**展开为卡片
时宽高不同步**（宽先到、高后到，两只弹簧 ζ=0.7/0.55）产生非线性形变；
点击展开、点外部收回。占位：launcher 状态栏绘制 + `LIVE_ACTIVITY` 静态区。

### P1-4 锁屏时钟与壁纸视差（鸿蒙空间化）

- 分钟切换（`LOCK_MINUTE`，`main.rs:6565`）瞬变 → **旧数字上移 12px 淡出、
  新数字下移入场 320ms `Emphasized`**；
- 锁屏拖动时壁纸反向位移 0.06x、时钟放大 1.0→1.03，产生深度视差；
- 解锁转场中时钟做 **scale 1.0→1.12 + alpha→0**（先扩后散，鸿蒙解锁质感）。

### P1-5 AOD → 锁屏连贯动效（ColorOS AOD 连续体验）

息屏显示低亮度时钟/日期帧；按键/抬手亮屏时 AOD 元素**原位放大 1.0→1.35 +
模糊淡出 350ms `Emphasized`**，同时锁屏时钟淡入，两者构图对齐形成「同源
生长」。依赖电源管理（`docs/power_management.md`）的息屏路径，动效本身
只需一帧 AOD 渲染 + 逐帧过渡。

### P1-6 应用下拉刷新（通用组件）

shared/motion.rs 附带 `PullRefresh`：跟手 1:1 → 越界 0.35x 阻尼（与翻页
橡皮筋一致）→ 松手弹簧回弹 + 顶部玻璃圆点涟漪 loader（三圆点错峰缩放，
周期 900ms）。先接 sms（会话列表）、mail、store（可用更新）。

### P2-1 非线性形变 squash & stretch（鸿蒙非线性形变）

图标按压 0.85 缩放改为 **x 0.92 / y 0.82 不等比**（挤压感），松开弹簧恢复时
x/y 反相位过冲；启动瓦片放大给 y 方向 1.03 倍瞬时超前（利用现有 spring 的
x/y 分离补间）。

### P2-2 光随指动（鸿蒙 7 空间光效）

`shared/glass.rs` 顶部镜面高光改为**跟随最近触摸点偏移**（高光中心 =
面板左上 + 触摸点相对位移 × 0.15，120ms `FastOutSlowIn` 跟随）；控制中心
磁贴按压时以按压点为圆心做**径向光晕扩散**（需给渐变模块加径向渐变直写，
现仅垂直渐变）。

### P2-3 最近任务 3D 纵深（鸿蒙多任务透视）

`draw_recents` 的卡片加**伪 3D**：距焦点第 n 张卡 scale = 1 - 0.08n、
y 下移 12n px、alpha = 1 - 0.15n，滑动时各卡位差值随进度连续插值（无旋转
矩阵，纯缩放近似，符合现有绘制能力）。

### P2-4 动画参数平滑接续（鸿蒙 springMotion overlapDuration）

`Spring` 增加 `retarget(now, new_target, inherit: (x, v))`：打断时把旧动画
当前值/速度喂给新弹簧（翻页初速继承已有此逻辑，`main.rs` 翻页回弹路径），
推广到控制中心、文件夹、锁屏；tween 打断转 spring 时用当前速度做初速。

### P2-5 帧率观测

`input_stack.rs:194` 已按 `anim::active()` 门控；补一个**动画帧耗时直方图**
（P50/P95 写 `/proc/anim_stat`），纳入 `docs/perf-baseline.md` 回归，防止
新动画把 120Hz 帧预算（8.33ms）打爆。

## 4. 改进项（现有动画打磨）

| 点 | 现状 | 改进 |
| --- | --- | --- |
| 返回转场快照 | 最后一帧静态快照缩回 | ColorOS 用实时缩略图：退出瞬间起每帧重绘快照（应用已退出，重绘成本低） |
| 启动白闪 | 线性后段 0→255 | 改 `Emphasized` 前段加速，减白闪突兀感 |
| 撕裂打断 | 仅启动转场 | 推广到返回转场中途再启动（连续打断，ColorOS 里程碑特性） |
| Toast 保持 | 固定 ~1.67s | 文字超 1 行按字数延长，退场加上滑 8px（入场的反向完整性） |
| 磁贴按压 | CC_PRESS_SPRING 均匀缩放 | 加按压位移 1px + 光晕（见 P2-2） |
| OOBE 常驻动画 | 呼吸已有 | 步骤切换时进度点加弹性过冲（ζ=0.6，150ms） |

## 5. 建议实施顺序

1. `shared/motion.rs` 运动库（P0-1）→ 2. 页面转场批量迁移（P0-2/P0-6）→
3. 系统小件（P0-3/P0-4/P0-5）→ 4. 打断框架（P1-1）→ 5. 共享元素（P1-2）→
6. 流体云（P1-3）→ 7. 锁屏/AOD（P1-4/P1-5）→ 8. 打磨（P2 全部）。
每步保持 `docs/animations.md` 数值同步更新。

## 参考

- [ColorOS 15 官网（极光引擎/并行动画）](https://www.coloros.com/article/A00000066/)
- [ColorOS 15 版本主页](https://www.coloros.com/version/coloros15)
- [OPPO HK 幻光影流动/AOD 连贯动效](https://www.oppo.com/hk/coloros15/)
- [ColorOS 15 上手评测（打断动画）](https://zhuanlan.zhihu.com/p/15072930250)
- [鸿蒙共享元素转场（一镜到底）官方指南](https://developer.huawei.com/consumer/cn/doc/harmonyos-guides/arkts-shared-element-transition)
- [鸿蒙弹簧曲线 springMotion 参数](https://developer.huawei.com/consumer/cn/doc/harmonyos-guides-V14/arkts-spring-curve-V14)
- [HarmonyOS 7 百度百科（HDC 2026）](https://baike.baidu.com/item/HarmonyOS%207/67704957)
