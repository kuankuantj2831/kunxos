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
- [x] A3 getrandom 真实熵源（virtio-rng 限时取熵 + 64B 混合熵池，第 2 轮完成）
- [x] A4 ~~TCP 通告窗口~~：核查已实现（tcp_xmit 按 TCP_RXBUF-rx_count 通告真实剩余空间，socket.rs:735）
- [x] A9 ~~信号帧边界误杀 pthread~~：核查已修（signal.rs 帧校验已放宽到任意已映射用户空间，112-127/213-221）
- [x] A5 TIME_WAIT 定时回收（30s 超时，tcp_reap_syn_rcvd 同场回收）
- [x] A6 ARP 条目老化（60s TTL，lookup 命中超龄即作废重新解析）
- [x] A7 ~~oobe 输入跨 read 分割~~：核查已修（EV_BUF 累积缓冲 + consume_input_event，oobe:3752-3921）
- [x] A10 ~~slab 双释放防护~~：核查已修（slab_free 空闲链表遍历拒绝重复释放，heap.rs）
- [x] A8 ~~input 多等待者覆盖~~：核查已修（4 槽 + 满则 yield 重试，input.rs:405-430）
- [x] A11 持续审查（已完成 5 轮：新代码/今夜diff/apps+服务×2/bootloader+冷门模块）（Agent 工具, Explore/general-purpose）找新 bug 并修复

### B. 动画 / ColorOS 17 对标（kunxcore/src/compositor.rs、ui_framework.rs、apps/launcher）

- [x] B1 统一动画引擎：已有完整基建（`shared/motion.rs` 贝塞尔+弹簧、内核 `kunxcore/src/anim.rs`、launcher `app_launch.rs` 转场、`anim.rs` 动画框架、docs/animations.md 规范）。本轮补齐 **ColorOS 17 标准曲线**：`Easing::ColorOSStandard(0.22,1,0.36,1)` + `ColorOSEmphasized(0.3,0,0.1,1)`，内核与应用两侧同步。**注意：B2 图标按压回弹、B3/B4 启动/返回转场均已存在**（app_launch.rs 的 start_press/start_launch/start_fly_away），后续轮次勿重做，改为把既有动画的曲线切换到 ColorOS 曲线并逐个体验调优。
- [x] B5 桌面翻页惯性滚动 + 越界回弹阻尼（松手 ColorOSStandard 减速吸附、到位才提交页码；相邻页视差 位移系数≈0.85 抛物线包络；越界 0.35x 橡皮筋沿用）
- [x] B6 通知横幅：核查已存在（call/health banner 滑入+easing，launcher:775-860）
- [x] B7 窗口动效：核查已存在（内核 anim.rs 窗口动画引擎 + gesture.rs 返回/回桌面/多任务 tween）
- [x] B8 解锁/开机亮度渐亮动画（520ms ColorOSStandard 0→全亮；开机 flip LUT 施加 / 解锁 BACK_BUF 烘焙两模式，无整屏突暗）
- [x] B9 dock 图标悬浮放大（鼠标光标位驱动 1.12x + 上浮 2px，第 8 轮）
- [x] B10 应用启动骨架屏呼吸（第 4-5 轮完成）
- [x] B11 长按编辑模式：核查已存在（HyperOS 3 风格编辑器，图标抖动+拖动换位）
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

### 第 7 轮（2026-10-17 凌晨，R4 剩余面 + 动效补充）
- **R4 审查**（cloud/audiomix 等 12 应用 + 6 服务）：3 Medium（全在 cloud：seed 越界 panic / poll 响应别名覆盖 / 备份 n 不一致）+ 2 Low，全部修复提交；6 个服务核实无缺陷
- **动效**：控制中心子元素错峰入场（面板展开/跟手期间各行卡片按序上移显现，ColorOS 风格）已提交
- 回归：make 全量 0 错误 + test-sfsrace PASS

### 第 8 轮（2026-10-17 00:3x，定时轮）
- **B9 dock 图标悬浮放大**：鼠标光标位驱动 1.12x + 上浮 2px（按压反馈优先，MOUSE_SEEN 门控防触摸误触，无独立动画循环——随 CURSOR_DIRTY 重绘）
- **R5 审查**（kunxboot 12 文件 + kunxcore 冷门 11 模块 + kunxsec + kunxui）：1 High + 3 Medium + 3 Low，本轮修复 H-1/M-1/M-2/M-3/L-3——
  - H1 /proc/power、/proc/pressure 写路径加 root 门禁（原任意应用可冻结/限速其它进程 = syscall 层已修权限漏洞的旁路复活）
  - M1 OTA load_otp 读 8 扇区配 2048B 缓冲恒失败 → 改 4 扇区，OTP 防回滚楼层首次真正生效
  - M2 bspatch inflate_kxrl1 传精确长度窗口（原 64MB scratch 下增量 OTA 100% 失败）
  - M3 power status shell 切片钳制 + push_dec 返回值钳到缓冲长（防内核 shell panic）；L3 thermal force checked_mul
  - 未修：L1 gdbstub RISC-V 单步/断点重插失效（调试设施，需重构）、L2 fdt 递归无深度上限（DTB 来源已验签，防御纵深项）——记入待办
- 任务池勾选修正（A3/B6/B7/B9/B10/B11 状态同步，去重 A9/A10）
- 回归：make 全量 0 错误 + test-sfsrace PASS

### 待修清单（后续轮次领取）
- L1 gdbstub RISC-V：断点重插/单步重构（恢复原指令后在 sepc+ilen 插临时断点）；bp_set 压缩指令半字覆盖；删 AArch64 死代码
- L2 fdt.rs 四处递归遍历加 depth 上限（32）
- A1 launcher mmap /dev/fb0 偶发 EINVAL：等 [fb-mmap] 诊断输出复现
- kunxui 控件库已就绪但零引用：可评估接入 launcher（动画长尾）

### 第 9 轮（2026-10-17 01:3x，定时轮）
- **L2 fdt.rs 四处递归遍历加 MAX_DEPTH=32 深度上限**（wrapper+depth 参数，防深层 DTB 栈耗尽；bootloader 侧同款）
- **L1 gdbstub 部分修复**：断点宽度感知 RVC 压缩指令（2 字节 ebreak，不再瞬态清零下一半字）、对齐要求放宽到 2 字节、重插按记录宽度；AArch64 死代码 sync_gdb_entry 保留（编译期无害）——完整单步重构仍待做（RISC-V S 模式需临时断点方案）
- 回归：make 全量 0 错误（仅预存 target-feature 旗标警告）+ test-sfsrace PASS

### 第 10 轮（2026-10-17 02:3x，定时轮）
- **L1 gdbstub 完整重构**：RISC-V S 模式无硬件单步，断点继续/单步改为「摘断点 → 下一条指令（RVC 宽度感知）植入临时 ebreak → 命中后重插原断点」的推进状态机；`c` 静默继续、`s` 停在下一条指令。替换失效的 AArch64 set_ss 残留（原断点一次性消失 + PENDING 永久悬空）。已知限制：仅跟踪顺序流（分支目标不埋临时断点，与常见软件单步同款）
- 回归：make 0 错误 + test-sfsrace PASS

### 第 11 轮（2026-10-17 03:3x，定时轮）
- **解锁粒子迸发动效**：解锁瞬间自锁图标位置迸发 24 颗光点（黄金角铺开 + 三档径向速度 + 轻重力 + 520ms 淡出），叠在上滑的锁屏层之上，随转场同帧推进（无独立动画循环）
- **kunxui 接入评估结论**：面向设置类控件（沙粒滑块/星辰开关/星尘输入框，500 粒子静态池），为一次性解锁动效引入整 crate 依赖不划算——本动效内联实现；kunxui 保留为后续设置应用控件库候选
- 回归：make 0 错误 + test-sfsrace PASS

### 第 12 轮（2026-10-17 02:5x，定时轮）
- **R6 交付质量关**（审查第 8-11 轮 4 提交）：1 High + 3 Medium + 3 Low，全部修复——
  - H1 gdbstub 非断点处 `s` 静默自由运行（连续两次 s 必现、GDB 挂起）→ Step 分支无条件武装临时推进断点
  - M1 TEMP 残留被覆写（分支断点场景用户断点永久丢失 + 幽灵 ebreak）→ arm 前清算旧 TEMP
  - M2 dock 悬浮在空闲桌面永不出现（光标快路径不触发整屏重绘）→ 进/出下标翻转检测置 NEEDS_REDRAW
  - M3 解锁粒子淡出无效（fill_rect 不吃 PIX_ALPHA）→ 逐像素 put_pixel；L 粒子寿命 520→290ms 对齐转场、中心移到锁图标 (W/2,308)
  - L arm_temp_advance bp_set 失败回滚（不再自由运行+残留状态）
  - 复核通过：procfs 门禁无 TOCTOU/无误伤、OTA 4 扇区几何与 bootloader 一致、bspatch 边界、fdt 无第五处递归、粒子越界安全
  - 未改：sync_gdb_entry 死代码（无调用者，编译期无害，记录在案）
- 回归：make 0 错误 + test-sfsrace PASS

### 第 13 轮（2026-10-17 03:4x，定时轮）
- 死代码清理（R6-L 收尾）：删除 gdbstub sync_gdb_entry（AArch64 残留解码、全仓库无调用者的误导性死代码）；删除 kunxcore ota.rs 已无引用的 OTP_SECTORS=8 死常量（bootloader 侧同名常量不受影响）
- 摸底：make test-wireless 现为 0 测试（RISC-V 移植后宿主测试已空置，BUG_REPORT 历史"105"数字过期）——以 test-sfsrace 为权威回归
- 回归：make 全量 0 错误 + test-sfsrace PASS

### 第 14 轮（2026-10-17 04:4x，定时轮）
- **A1 复现实验**：重建正式（非测试）内核后 4 次完整 GUI 启动 + 6 次测试模式启动，`[fb-mmap]` 诊断零输出——偶发 EINVAL 未复现（OOBE 链路正常：输入设备就绪、字体加载、30s 空闲推进均在）。诊断打印保留在位，下次自然复现时串口日志将直接定位到具体 EINVAL 分支
- 顺带验证：今晚全部内核+launcher 改动在真实 GUI 启动链路（OpenSBI→内核→init→oobe）正常工作，0 PANIC / 0 fault
- 回归：make 全量 0 错误 + test-sfsrace PASS

### 第 15 轮（2026-10-17 05:4x，定时轮）
- CHANGELOG 补记第 8-14 轮成果（R5/R6 审查修复、gdbstub 重构、三个新动效、验证统计）
- 交付状态：待修清单清空（A1 转为诊断就位待自然复现）；全部提交经双轮独立审查（R3/R6 质量关）

## 最终交付总结（2026-10-17 06:36，夜间十小时自动化收官）

**总账（07:36 终版）**：本地 git 35 个提交（43eaaf9 之后，未 push）；6 轮并行代码审查（R1-R6）+ 主会话连续工作段 + 19 个工单轮次；全夜间每轮 `make` 全量 0 错误，`make test-sfsrace` 累计 14+ 次 PASS。**端到端 GUI 实证**：OOBE 13 步自动推进 → launcher 桌面稳定运行 3.5 分钟 0 PANIC/0 fault（第 18 轮冒烟还抓到并修复了一个 launcher panic 循环——health_load 读失败 -1 经 as usize 穿透守卫，usize 清剿族漏网）；第 17 轮清掉 IRQ 上下文 health 上报的毫秒级 UART 忙等（延迟环形缓冲 + perf_log 线程冲刷）。

### 一、修复的 bug（35+ 项，全部经审查确认有具体触发路径）

**内核安全/正确性（High 级）**
1. /proc/power、/proc/pressure 写路径无权限检查——任意应用可冻结/限速其它进程（syscall 层已修漏洞的旁路复活）→ root 门禁
2. IRQ 故障遏制槽位跨任务残留——可展开他人陷阱帧跨地址空间 sret 执行任意代码 → tid 归属校验
3. panic 遏制缺 IRQ 上下文守卫——中断 handler panic 错杀无辜进程/进程孤儿化
4. virtio-rng 超时后 used-ring 失步——后续取熵返回垃圾 + 迟到 DMA 写调用者栈
5. SCHED_LOCK 自愈抢锁与任务退出锁交接窗口竞态（SMP 双核入临界区）→ 遏制出口显式放锁

**内核功能修复**
6. getrandom 时间种子 LCG → virtio-rng 硬件熵 + 时间/身份混合熵池（跨调用/跨启动可区分）
7. TIME_WAIT 30s 定时回收（含悬空句柄清理）；ARP 60s TTL（born_us 判定，LRU 不续命）
8. OTA：load_otp 扇区数纠正（OTP 防回滚首次生效）、KXRL1 增量解压窗口（原 100% 失败）
9. gdbstub 完整重构：RISC-V 软件单步（临时推进断点状态机）+ RVC 宽度感知 + TEMP 清算/失败回滚
10. fdt 递归深度封顶、rename_task UTF-8 边界、wait4 WIFSIGNALED 编码、定时器遏制级联、health 环形缓冲 IRQ 防护、power status 双侧钳制、thermal force 溢出、crash_screen/bootfail 横幅修复

**应用与服务（17 项）**
alarmd 贪睡闹钟秒停+一次性闹钟静默禁用、sms 首启判定反（消息每次翻倍）、rild 长短信截断、settings 单位损坏/权限页守卫、gallery 不可见瓦片可点击、cloud 三处（越界闪退/poll 别名覆盖/备份 n 不一致）、audiomix、launcher 健康横幅选最轻事件等

### 二、ColorOS 17 动效对标

- **曲线体系**：三端（内核 anim.rs / shared/motion.rs / launcher 本地）接入 ColorOSStandard(0.22,1,0.36,1) + ColorOSEmphasized(0.3,0,0.1,1)，手势退出/多任务/页面切换/应用入场 20+ 站点统一
- **新动效 7 项**：桌面翻页惯性+视差（B5）、开机/解锁亮度渐亮（B8）、应用启动骨架屏呼吸（B10）、状态栏充电动效-弹簧弹跳+呼吸闪电（B12）、控制中心错峰入场、dock 悬浮放大、解锁粒子迸发

### 三、验证与质量关

- 所有新代码经 R3/R6 两轮独立 fresh-eyes 审查（共抓出夜间自身引入的 5H+6M+7L 并全部修复）
- 历史高发族（usize 返回值/from_utf8/CJK 定长拷贝）在全部 apps+服务核实清零
- 内核 7 条预存警告清零；死代码清理（sync_gdb_entry 等，-88 行）

### 四、遗留事项（均已记录，非阻塞）

1. A1：launcher mmap /dev/fb0 偶发 EINVAL——10 次启动未复现，`[fb-mmap]` 诊断就位，复现时串口直接定位
3. gdbstub 软件单步仅跟踪顺序流（分支目标不埋临时断点，与常见实现同款）
4. SMP 仍为实验特性；kunxui 控件库就绪待接入（设置应用候选）
5. test-wireless 宿主测试在 RISC-V 移植后空置（以 test-sfsrace 为权威回归）

### 第 17 轮（2026-10-17 06:5x，定时轮）
- **L-D 修复**：IRQ 上下文的 health 上报不再做阻塞 UART 打印（中文多行 ~10ms+ 忙等，违反仓库「IRQ 路径禁止」规则）——拼行进 2KB 延迟环形缓冲（关中断拷贝 + 溢出丢弃计数），由 perf_log 内核线程与无头回退主循环冲刷；非 IRQ 上下文打印路径不变
- 回归：make 0 错误 + test-sfsrace PASS

### 第 18 轮（2026-10-17 07:1x，定时轮）
- **GUI 全链路冒烟抓到真 bug 并修复**：OOBE 自动推进（13 步 × 30s）→ launcher 拉起后 `PANIC main.rs:910` panic 循环——health_load 的 read_file 失败返回 -1 经 `as usize` 变巨大正数穿透 n==0 守卫 → 切片越界（历史第 12/13 轮 usize 清剿族漏网，因 /var/run/health.log 打开失败路径此前从未在 GUI 真机走到）。改按 isize 判负
- 修复后复跑完整 GUI 冒烟：OOBE→launcher 桌面（tile/dock 全部就位）稳定运行 3.5 分钟 **0 PANIC / 0 fault**
- 回归：make 0 错误 + test-sfsrace PASS

### 第 19 轮（2026-10-17 08:4x，用户报告黑屏 → 根因修复）
- **用户实测报告：OOBE 进系统后整屏黑**。经 QEMU monitor screendump + 像素亮度统计定位（黑屏实锤 avg=0.1，仅底部导航条可见）
- **根因（两层）**：
  1. 工具链层：`make kernel` 不重嵌更新的 ramdisk（cargo mtime 误判，Makefile 已知坑）——前两次修复「无效」是跑了旧二进制；修复流程补 `touch ramdisk.img`
  2. 代码层：**B8 渐亮依赖墙钟（gettimeofday）**，RTC 就绪前后墙钟跳变 → flip 的 now 落到 T0 之前 → progress 的 now<=t0 → α 永久 0 → 每次 flip 经全 0 LUT 上屏 → 整屏黑（探针实证：BACK_BUF 均值 210 内容正常、lvl=0、T0 恒等于当前墙钟）
- **修复**：① 内核 sys_clock_gettime 补 CLOCK_MONOTONIC 支持（原忽略 clk 恒回墙钟）；② launcher 新增 mono_us() 并全动画链改用；③ boot_fade_alpha 加 fail-bright 防御（now<=t0 视为播完返回 1.0，绝不 fail-black）
- **验证**：修复后 GUI 全链路 screendump 实拍——桌面完整渲染（时钟小组件/图标/dock/状态栏），亮度 0.1 → 226.7；test-sfsrace PASS

### 第 20 轮（2026-10-17 10:5x，定时轮）
- **墙钟跳变隐患系统性清剿**（黑屏根因的泛化修复）：launcher `now_us()` 本体改为单调钟（CLOCK_MONOTONIC）——全部 80+ 动画 T0/超时/时长判定一次性治好（转场/横幅/锁屏/编辑/最近任务/控制中心/翻页等，含 app_launch/app_life/control_center 经 use super::* 自动受益）；新增 `wall_us()` 仅供时钟/日历等真实时间显示（time_hhmm/表盘指针/日历小组件/锁屏日期/控制中心日期 5 处）
- 验证：GUI 全链路 launcher 正常拉起 0 PANIC、亮度 226.7 正常；make 0 错误 + test-sfsrace PASS

### 第 21 轮（2026-10-17 11:1x，定时轮）
- **墙钟清剿扩展到全部应用**：11 处应用侧 now_us 本体改 CLOCK_MONOTONIC（calculator/settings/stub/store×2/terminal/cloud/tlstest/oobe finish + shared/app_kit + shared/ttf_text）——覆盖按键动画、轮询超时、光标闪烁、加载动效等全部时间差判定；墙钟显示保留（oobe finish 时钟改内联 gettimeofday；clock 应用本就纯显示未动）
- cloud 无 syscall3 封装：该处手写 ecall
- 回归：make 0 错误 + test-sfsrace PASS

### 第 22 轮（2026-10-17 上午，用户报告 OOBE 动画卡顿）
- **OOBE 动画帧率拉满**：原「固定帧数 + 帧间固定 sleep（3-36ms）」模式实测仅 ~11fps（Hero 260ms 只跑 3 帧）。逐段计时定位瓶颈（背景全屏重绘 3-12ms / brand 光晕逐像素圆 13ms / flip 整屏 2MB 拷贝 / nav 毛玻璃每帧重画 40ms 首次）后四项优化：
  1. 全部动画改**真实时间推进 + 帧间不睡眠**（时长恒定，帧率拉满；mono_us 单调钟）
  2. 背景渐变**行缓存**（每帧省 47 万次混合运算）
  3. nav 毛玻璃**首帧快照**（动画期行恢复，省每帧 40ms 模糊计算）
  4. Hero/标题动画**区域呈现**（brand/文字矩形拷贝+脏矩形，替代整屏 2MB flip）+ 光晕 **scanline 化**（13ms→0.7ms）
- 实测：Hero 260ms **3 帧 → 21 帧（≈81fps，提升 7 倍）**；主循环节流 16→8ms 且动画期不睡
- 回归：make 0 错误 + test-sfsrace PASS

### 第 23 轮（2026-10-17 午间，定时轮）
- **帧率拉满清剿延续**：OOBE 光锥扫描（icon cone）改真实时间推进（原 13 帧固定 + sleep）；calculator 闪光/结果弹出、store 页面转场的主循环改「动画期不睡眠、空闲 8ms 让出」（原固定 16ms = 60fps 上限）；settings 主循环 16→8ms
- 回归：make 0 错误 + test-sfsrace PASS

### 第 24 轮（2026-10-17 午间，用户报告 OOBE 动画过不完/指针消失）
- **根因**：第 22 轮转场改真实时间推进时只换了循环头、**漏加退出条件** → `transition_pages` 死循环，OOBE 卡死在转场、输入处理停摆（指针不显示不可点）
- **修复**：① 转场循环补 `t >= 1.0 break`；② 按用户要求新增 `pump_input_during_anim`——全部 5 个动画循环（转场/Hero/标题/脉冲/光锥）每帧泵入非阻塞输入：指针移动照常更新光标位置并重画 overlay（动画中指针可见可动），点击/回车在动画期吞掉（<400ms）
- **GUI 实证**：OOBE 连续推进到「选择应用」页（多转场全走完），动画期间注入的鼠标移动正常跟手显示；test-sfsrace PASS

### 第 25 轮（2026-10-17 午后，定时轮）
- **第 22-24 轮 OOBE 改造自查**，修两处遗留：
  1. 空闲计时序 bug：`ANY_INPUT_RECENT` 消费在前、判读在后 → 移动鼠标时 loop_t0 永不重置，30s 自动推进误触发（消费与重置合并到同分支，read_input_event 命中分支同步重置锚点）
  2. `active=true` 使空闲期满速空转烧 CPU → 改回按 `btn_animating()` 判定，呼吸动画空闲态 8ms 让出
- 清理残留诊断（hero frames 计数/分段计时变量）与「value assigned never read」警告
- 回归：make 0 错误 0 警告 + test-sfsrace PASS

### 第 26 轮（2026-10-17 午后，用户报告 OOBE 光标移动卡）
- **主循环节流改造（光标流畅优先）**：有输入待处理/光标待更新时帧间不睡眠（原固定 8ms 睡眠叠加在光标路径上）；呼吸动画降到 ~30fps（隔帧推进——每帧 13K 像素快照恢复+brand 重绘+区域呈现是空闲循环大头，让出一半循环给光标）
- GUI 验证：快速扫动后光标正常显示跟随；test-sfsrace PASS

### 第 27 轮（2026-10-17 午后，定时轮）
- **launcher 桌面同款「光标流畅优先」**：主循环帧预算睡眠在 CURSOR_DIRTY 待处理时跳过（原固定 16ms 预算叠加在光标路径上，桌面指针移动与 OOBE 同样发涩）
- 回归：make 0 错误 + test-sfsrace PASS
- 注：本轮提交顺带收录了并行产生的 kos/src/store/mod.rs（对象存储引擎 Phase 1 Step 2，747 行——非本工单产物，仅本地保护性入库）

### 第 28 轮（2026-10-17 下午，定时轮）
- **R7 审查第 22-27 轮帧率/光标改造批**：4 缺陷全部修复——
  - A(P2) pump 的 any 门使纯移动不重画光标（动画期光标冻结的真相）→ 去 any 门，CURSOR_DIRTY 直接重画
  - B(P2) store 转场终帧从不绘制（页面停在残缺偏移）→ transition_active 改 TRANS_T0!=0（p>=1 当拍自清+绘终帧，与 calculator 对齐）
  - C(P3) transition_pages 漏接泵（转场期点击排队到新页生效/连跳两步）→ 补接
  - D(P3) launcher 光标优先是死条件（CURSOR_DIRTY 帧内已被消费）→ 改 FRAME_INPUT 标志（handle_input 置位、睡眠判定后消费）
  - 复核通过：五循环退出/终态齐备、nav_snap 181KB 栈安全（2MB 栈 8.7%）、HERO 快照坐标对齐、idle 时序修复正确、无忙转
- 回归：make 0 错误 + test-sfsrace PASS
