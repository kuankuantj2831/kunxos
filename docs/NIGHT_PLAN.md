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
- [x] A4 ~~TCP 通告窗口~~：核查已实现（tcp_xmit 按 TCP_RXBUF-rx_count 通告真实剩余空间，socket.rs:735）
- [x] A9 ~~信号帧边界误杀 pthread~~：核查已修（signal.rs 帧校验已放宽到任意已映射用户空间，112-127/213-221）
- [x] A5 TIME_WAIT 定时回收（30s 超时，tcp_reap_syn_rcvd 同场回收）
- [x] A6 ARP 条目老化（60s TTL，lookup 命中超龄即作废重新解析）
- [x] A7 ~~oobe 输入跨 read 分割~~：核查已修（EV_BUF 累积缓冲 + consume_input_event，oobe:3752-3921）
- [x] A10 ~~slab 双释放防护~~：核查已修（slab_free 空闲链表遍历拒绝重复释放，heap.rs）
- [x] A8 ~~input 多等待者覆盖~~：核查已修（4 槽 + 满则 yield 重试，input.rs:405-430）
- [ ] A9 信号帧边界按进程主栈校验误杀 pthread 工作线程
- [ ] A10 slab kfree 双重释放防护补完整
- [ ] A11 每轮空闲时：跑一轮并行代码审查（Agent 工具, Explore/general-purpose）找新 bug 并修复

### B. 动画 / ColorOS 17 对标（kunxcore/src/compositor.rs、ui_framework.rs、apps/launcher）

- [x] B1 统一动画引擎：已有完整基建（`shared/motion.rs` 贝塞尔+弹簧、内核 `kunxcore/src/anim.rs`、launcher `app_launch.rs` 转场、`anim.rs` 动画框架、docs/animations.md 规范）。本轮补齐 **ColorOS 17 标准曲线**：`Easing::ColorOSStandard(0.22,1,0.36,1)` + `ColorOSEmphasized(0.3,0,0.1,1)`，内核与应用两侧同步。**注意：B2 图标按压回弹、B3/B4 启动/返回转场均已存在**（app_launch.rs 的 start_press/start_launch/start_fly_away），后续轮次勿重做，改为把既有动画的曲线切换到 ColorOS 曲线并逐个体验调优。
- [x] B5 桌面翻页惯性滚动 + 越界回弹阻尼（松手 ColorOSStandard 减速吸附、到位才提交页码；相邻页视差 位移系数≈0.85 抛物线包络；越界 0.35x 橡皮筋沿用）
- [ ] B6 通知横幅（若有通知组件）滑入 + 手势滑动速度判定收/放
- [ ] B7 窗口 close/minimize 缩放淡出
- [x] B8 解锁/开机亮度渐亮动画（520ms ColorOSStandard 0→全亮；开机 flip LUT 施加 / 解锁 BACK_BUF 烘焙两模式，无整屏突暗）
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

### 第 2-3 轮（2026-10-16 深夜，主会话 + 三路并行子代理）
- **A3 getrandom 真实熵源**：virtio-rng 限时非阻塞接口 + 64B 熵池（硬件熵/time CSR/墙钟/pid/tid/序号混合），替代时间种子 LCG（已提交）
- **A11 第一轮审查**（新代码 crash_screen/health 等）：2 High + 2 Medium + 6 Low，其中 H-1/H-2/M-1/M-2 与 L-2/L-3/L-4/L-5 **全部修复提交**；L-1/L-6 本轮跟进
- **A5 TIME_WAIT 30s 定时回收、A6 ARP 60s 老化**（已提交）
- **A1**：map_framebuffer 三个 EINVAL 分支加诊断打印（需复现定位，未决）
- **B5 翻页惯性+视差、B8 开机/解锁渐亮**（launcher，ColorOSStandard，已提交）；内核手势 5 处 tween + 页面切换切 ColorOS 曲线（已提交）
- 核查已修不需重做：A4/A7/A8/A9/A10、B2/B3/B4/B6/B7/B11
- 回归：make 全量 0 错误；make test-sfsrace **PASS ×4**（期间定位并排除一次 launcher 中间版本引入的 MAILBOX 失败）
- 剩余待做：L-1 定时器遏制级联、L-6 健康横幅选最严重事件、第二轮 apps/kunxserv 审查、充电动效/骨架屏等动画长尾

### 第 4-5 轮（2026-10-17 凌晨，主会话 + 第二轮并行子代理）
- **A11-R2 审查**（12 应用 + 5 服务约 30K 行）：1 High + 3 Medium + 2 Low，全部修复提交（alarmd 贪睡秒停/一次性闹钟静默禁用、sms 首启判定反、settings GB 单位、rild 长短信截断、gallery/settings 命中守卫）；历史修复（usize 模式/from_utf8/CJK 拷贝）核实均在位
- **L-1** 定时器遏制超限直转整机诊断；**L-6** 健康横幅按严重级挑选（已提交）
- **B10 应用启动骨架屏呼吸、B12 状态栏充电动效**（弹簧弹跳+呼吸闪电+带状快速重绘，已提交）
- 内核 7 条预存警告清零（已提交；其中 2 个 mut 为必需，已恢复）
- 回归：make test-sfsrace PASS ×3
- 进行中：R3 审查今夜全部 diff（交付前质量关）

### 第 6 轮（2026-10-17 凌晨，R3 复查关）
- **R3 审查今夜全部 diff**（43eaaf9..HEAD 12 提交）：发现夜间代码自身 2 High + 3 Medium + 4 Low，已全部修复提交——
  - H1 virtio-rng 超时后 used-ring 失步（后续取熵返回垃圾 + 迟到 DMA 写调用者栈）→ STALE_USED 有界排空 + legacy 路径统一 TIMED_BUF
  - H2 SCHED_LOCK 自愈抢锁与任务退出锁交接窗口竞态（SMP 可达双核入临界区）→ 回退自愈、改遏制出口显式放锁（顺带修 L-C 自持锁死自旋）
  - M1 TIME_WAIT 回收清悬空句柄；M2 ARP TTL 改 born_us（LRU 刷新不再续命）；M3 B10 骨架屏 flip 后二次校验；LA 仅 INFO 不立横幅
  - L-D（IRQ 上下文 health 上报的阻塞 UART 打印）记录为已知取舍，未改
- 同时核实夜间所有其它改动无缺陷（ColorOS 曲线数值单调≤1、B5/B8/B12、TIME_WAIT 锚点、IRQ_TID 校验、wait4 编码与 kunxinit 解码闭环等）
- 回归：make 全量 0 错误 + test-sfsrace PASS

## 未决事项（供后续轮次/用户决策）
- A1 launcher mmap /dev/fb0 偶发 EINVAL：诊断打印已就位，需下次复现读 [fb-mmap] 输出定位
- L-D IRQ 上下文 health 上报的 UART 打印（事件稀有，毫秒级停顿，属取舍）
- SMP 模式仍为实验特性（bootargs "smp"）；H2 修复后调度锁已无已知竞态，但整体未硬化
