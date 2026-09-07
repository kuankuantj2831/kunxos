# 对标 ColorOS 15：P0 四项动画实现计划

## 背景
探索确认：launcher 已有 Spring（阻尼谐振子，`app_launch.rs`）+ Easing（8 种贝塞尔曲线，`main.rs:4948`）基建，且控制中心/文件夹/应用转场均已接入。但以下四处为瞬切/静态，是 ColorOS 15 对标下的最大缺口：

| 缺口 | 现状 |
|---|---|
| 桌面翻页 | `CURRENT_PAGE` 改变后直接重绘（`main.rs` Swipe 分支） |
| 最近任务 | `RECENTS_OPEN` 瞬切，卡片无入场/无清理动画（`draw_recents`） |
| 锁屏解锁 | `unlock_success()` 瞬切重绘（`main.rs:1170`） |
| Toast | 静态显示 2s（`draw_overlay`，`main.rs:2246`） |

## 改动文件
- `apps/launcher/src/main.rs`（主要）
- `apps/launcher/src/app_launch.rs`（新增 `Spring::page()` 弹簧参数）
- `docs/animations.md`（新增 3.7–3.10 规格章节，保持「规范权威来源」惯例）
- `CHANGELOG.md`

## 1. 桌面翻页动画（跟手 + 弹簧吸附 + 页码点过渡）
- 新状态：`PAGE_DRAG`、`PAGE_OFFSET: f32`（像素偏移）、`PAGE_SPRING: Spring`（`Spring::page()`：stiffness≈420，ζ≈0.85，与控制中心同风格）。
- **跟手**：桌面空闲态（非编辑/文件夹/重命名/最近任务/控制中心/锁屏）横向拖动 >24px 且横向位移占优时进入 `PAGE_DRAG`，`PAGE_OFFSET = clamp(dx)`（页边界外 0.35x 橡皮筋阻尼），实时重绘。
- **松手**：位移 > 1/4 屏或横向速度足够 → 更新 `CURRENT_PAGE`，`PAGE_OFFSET` 从 ±屏宽弹簧回 0；否则弹簧回 0（初速度继承滑动速度）。
- **绘制**：`draw_page` 改为 `draw_page_at(page, offset_x)`，当前页与相邻页各按偏移平移绘制（`layout_page` 复用，超屏剪裁）；壁纸/状态栏/Dock 不动（与 ColorOS 一致）。
- **页码点**：`draw_page_dots` 随 `PAGE_OFFSET/W` 整体平移，当前点/目标点大小插值过渡。
- 与现有手势优先级兼容：`cc_handle` → 锁屏 → 翻页 → 其余逻辑；`BOOT_ACTIVE`、`page_count()==1` 时禁用。

## 2. 最近任务面板动效（错峰入场 + 关闭淡出 + 上滑清理）
- **打开**：记录 `RECENTS_T0`，每张卡片延迟 `i*30ms`、200ms 上滑入场（y +24→0）+ 淡入（`EaseOut`），与 ColorOS 卡片错峰节奏一致。
- **关闭**：150ms 整体下滑 12px + 淡出后再 `RECENTS_OPEN=false`。
- **单卡上滑清理**：卡片上按住上拖 → 跟手位移；松手超阈值 → 200ms `FastOutLinearIn` 上飞淡出，完成后从 `RECENTS_LIST` 前移删除，剩余卡片 200ms 弹簧补位到新格位（记录每卡 `from_y→target_y`）；飞出中的卡不参与命中测试。
- 卡片绘制函数加 offset/alpha 参数（复用现有 `blend` 与 `draw_glass`）。

## 3. 锁屏解锁/锁定转场
- 新状态：`LOCK_PHASE {None, Unlocking, Locking}` + `LOCK_T0`，320ms。
- **解锁**（`unlock_success` 改造）：不再立即 `LOCKED=false`；动画期间先正常渲染桌面路径，再叠加锁屏层——锁屏内容（时间/日期/解锁控件）随 `FastOutSlowIn` 上移 ~140px 并整体淡出，完成后置 `LOCKED=false`。
- **锁定**（`lock_now` 改造）：桌面渲染后锁屏内容从 +60px 下滑淡入，完成后 `LOCKED=true`。
- **上滑跟手**（简版）：锁屏态上滑拖动时锁屏内容实时跟手位移，松手越过 -120px 触发解锁动画，否则 250ms 弹簧回弹。
- `render_lock_screen` 拆出「内容层」绘制（支持 offset + alpha 参数），时间文字颜色用现有 `blend` 过渡。

## 4. Toast 动效
- `show_toast` 记录 `TOAST_T0`；`draw_overlay` 改为：
  - 入场 180ms：底部上滑 16px + 淡入（`FastOutSlowIn`）；
  - 结束前 150ms：淡出。
- `draw_glass` 增加 alpha 变体（最终 `put_pixel` 前 `blend(BACK_BUF, …, alpha)`），玻璃底随进度淡入。

## 5. 文档与验证
- `docs/animations.md` 新增 3.7 翻页 / 3.8 最近任务 / 3.9 锁屏转场 / 3.10 Toast 规格表（曲线、时长、弹簧参数与现有章节同格式）。
- `CHANGELOG.md` Unreleased 记录本轮动效。
- 验证：`make kernel` 全量编译通过（launcher 重编并重打 ramdisk）；动画期间主循环接入现有 `NEEDS_REDRAW` 推进链（`motion_advance || folder_advance || cc_advance || …`），帧预算复用 16ms 机制。改动不涉及内核 syscall ABI、`no_std` 约束、无新增 crate；内核 `anim.rs` 自检数值不动。

## 明确不做（本轮）
P1/P2 项（截屏白飞、通知横幅、充电动画、长按菜单、音量 OSD、预测性返回、通知中心实体化）——待本轮验证后再迭代。