# 夜间自动化工作计划（2026-10-16 起，为期 10 小时）

> 本文件是夜间自动化工单。每轮执行者：读「进度日志」→ 从「任务池」领取未完成任务 →
> 实现 → `make -j$(nproc)` 验证 exit 0 → 更新进度日志 → 本地 git commit（不 push）。
> 禁止向用户请求验收；用户醒来后由最后一轮汇总成果。

## 总目标

1. 持续修 bug（优先 BUG_REPORT.md 中「未修/残留」项 + 新发现缺陷）。
2. 大量增加 UI 动画，全面对标 ColorOS 17 的动效语言：
   - 图标按压回弹（spring scale 0.92→1.0）
   - 应用启动连续性转场（图标位置放大展开到全屏，圆角收拢）
   - 返回时窗口缩小回图标位置 + 淡出
   - 桌面翻页滚动惯性 + 页面视差
   - 窗口最小化/关闭缩放淡出动画
   - 通知横幅滑入/滑出 + 松手速度判定
   - 充电动效、解锁进入动画（亮度渐亮）
   - 全局统一 easing：cubic-bezier(0.22, 1.0, 0.36, 1.0)（ColorOS 标准"回弹"手感）

## 任务池

### A. Bug 修复（按优先级）

- [ ] A1 launcher `mmap /dev/fb0` 偶发 EINVAL（BUG_REPORT 十一节"已知未决"）：复现 + errno 分支定位修复
- [x] A2 ~~syscall_dispatch 跨栈写~~：**核查结论（RISC-V 路径不适用）**——子进程陷阱帧副本放子栈顶部、a0=0 就地置位，经 el0_resume_entry 直达 syscall_return_restore，不回卷 dispatch；dispatch 写回的是当前任务自己的活栈帧。无跨栈写。
- [ ] A3 getrandom 无真实熵源：接入 virtio-rng 熵池混合
- [ ] A4 TCP 通告窗口/流量控制（十三节"明确不改"升级为正式实现）
- [ ] A5 TIME_WAIT 定时退出
- [ ] A6 ARP 条目老化定时清理
- [ ] A7 oobe/launcher 输入事件跨 read 分割丢事件
- [ ] A8 input 阻塞读多等待者覆盖（4 槽不够时）
- [ ] A9 信号帧边界按进程主栈校验误杀 pthread 工作线程
- [ ] A10 slab kfree 双重释放防护补完整
- [ ] A11 每轮空闲时：跑一轮并行代码审查（Agent 工具, Explore/general-purpose）找新 bug 并修复

### B. 动画 / ColorOS 17 对标（kunxcore/src/compositor.rs、ui_framework.rs、apps/launcher）

- [x] B1 统一动画引擎：已有完整基建（`shared/motion.rs` 贝塞尔+弹簧、内核 `kunxcore/src/anim.rs`、launcher `app_launch.rs` 转场、`anim.rs` 动画框架、docs/animations.md 规范）。本轮补齐 **ColorOS 17 标准曲线**：`Easing::ColorOSStandard(0.22,1,0.36,1)` + `ColorOSEmphasized(0.3,0,0.1,1)`，内核与应用两侧同步。**注意：B2 图标按压回弹、B3/B4 启动/返回转场均已存在**（app_launch.rs 的 start_press/start_launch/start_fly_away），后续轮次勿重做，改为把既有动画的曲线切换到 ColorOS 曲线并逐个体验调优。
- [ ] B5 桌面翻页惯性滚动 + 越界回弹阻尼
- [ ] B6 通知横幅（若有通知组件）滑入 + 手势滑动速度判定收/放
- [ ] B7 窗口 close/minimize 缩放淡出
- [ ] B8 解锁/开机亮度渐亮动画
- [ ] B9 dock 栏图标悬浮放大（hover/触摸接近）
- [ ] B10 骨架屏/加载占位动画（应用启动期间 launcher 占位页加呼吸动画）
- [ ] B11 长按图标抖动 + 删除模式（ColorOS 编辑桌面）
- [ ] B12 每完成一批动画：QEMU GUI 冒烟（make run-gui / launcher_test）验证不崩溃

### C. 每轮收尾（必做）

- [ ] `make -j$(nproc)` exit 0（0 新警告）
- [ ] 更新本文件进度日志
- [ ] git add -A && git commit（本地，不 push）
- [ ] 若某任务 2 轮仍无法完成：记录阻塞原因，换下一任务，不空转

## 进度日志（每轮追加）

### 第 0 轮（基线，2026-10-16 深夜）
- 基线 `make -j$(nproc)` exit 0（7 个预存警告，非本轮引入）
- 夜间自动化调度已建立（每小时一轮）

### 第 1 轮（2026-10-16 深夜，主会话）
- A2 核查：RISC-V 路径不存在跨栈写残留（详见任务池标注）
- B1：内核 `anim.rs` 与 `shared/motion.rs` 新增 ColorOSStandard / ColorOSEmphasized 两条 ColorOS 17 标准曲线
- 摸底结论：launcher 已有按压回弹、启动/返回转场、长按编辑抖动；后续动画工作重点是「切换曲线到 ColorOS + 补缺失动效（翻页惯性、通知横幅、亮度渐亮等）」
- 验证与提交见 git log
