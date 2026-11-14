# KunxOS 当前代码审计、动画缺项及 Linux / Android 对比

审计日期：2026-11-14。结论来自当前工作区源码、交叉编译检查、宿主复现和一次隔离 QEMU 回归。

**总体判断：已经有能运行的自研操作系统与桌面，但安全隔离、同步数据持久化和 Linux ABI 的完整性仍有明显缺陷。动画数量不少，主要欠缺的是普及、可打断、与真实功能接线，以及帧呈现质量的观测。**

这是一轮全仓扫描后针对高风险路径的深入审计，不能保证找出了全部 bug。没有对每个驱动、密码学原语、恶意输入组合和所有应用做完整形式验证或模糊测试。报告列出发现的问题，不代表已经修复。

## 1. 范围、证据与测试

- 扫描到 371 个项目 Rust 源文件、约 189,351 行；排除本地 build 缓存与 third_party 源码。统计是规模指标，不是逐行审查完成度。
- 工作区在审计开始时已有大量改动；审计期间 HEAD 更新为 `46b1cfb9efb594ccf08c49b2408fa44c34050906`。随后仍有 `apps/launcher/src/app_launch.rs`、`tools/run_windows.py` 与 `tools/test_display.py` 的外部改动。本轮没有修改系统实现，也没有提交、回滚或删除这些改动。
- 核心证据文件 SHA-256 保存在本地 `build/audit-2026-11-14/source-hashes.json`。结论绑定所检查源码，不保证其他聊天继续修改后的状态。

| 检查 | 实际结果 | 不能据此推断的内容 |
| --- | --- | --- |
| 53 个裸机 crate 的 `cargo check --offline --locked --target riscv64gc-unknown-none-elf` | 50 通过；probe / sleepy 被过期锁文件阻止；rild 的 Windows build.rs 编译失败 | check 不等于完整链接、镜像打包和运行通过 |
| 内核 RISC-V check | 通过 | 不代表安全隔离正确 |
| sync_tests | 57 个测试通过 | mock Fs 没覆盖真实分块截断；也漏测更新后重新加载 |
| kunxsec / wireless / JPEG / font_startup_host | 分别 39 / 105 / 7 / 2 个测试通过 | 协议逻辑通过不等于真实无线硬件可用；密码学向量通过不等于独立安全认证 |
| text_host / weather_json | 实际执行，分别输出 ALL L10N OK / ALL PASS | 不等于所有应用和全部语言均覆盖 |
| KOS 宿主测试 | 离线缺少 criterion，未进入测试执行 | 不能记作测试失败，也不能记作通过 |
| 内核强制 dead_code 诊断 | 编译成功；内核汇总 386 条 warning，其中包含目标特性等告警 | 不是 386 个独立 bug，更不是可直接删除的 386 段代码 |
| 隔离 QEMU display 回归 | PASS；重建桌面应用与内核，桌面、设置启动/返回、分辨率切换、光标路径通过 | 未覆盖所有应用的所有交互；stage 中部分服务来自现有 rootfs |

QEMU 的 `display/report.json` 记录：请求 144Hz，目标为 143,988 mHz，`hardware_vsync=false`；四次启动转场记录为 15 / 17 / 15 / 17 帧，返回为 11 / 44 / 42 / 13 帧。**这些是目标频率和动画帧计数，不能换算为“稳定实际 144fps”。** 字体初始化为 137 / 120 / 132ms。

SFS 宿主程序的 basic / large / wear / wrap / crash / tear / race / lockser / python 完整重跑通过，报告 total failures=0、ALL HOST TESTS PASSED，日志为本地 `sfs-complete.log`。初次无参运行最后因为默认 `/tmp/sfs_host_rootfs.img` 不存在退出，随后使用 Python 生成隔离镜像并重跑；不要把初次缺测试输入的 panic 解释为 SFS 实现损坏。

可执行复现：`python docs/audit-2026-11-14-repro.py`。它直接引用当前 pkg、sync 与 HostFs 源码，并抽取当前系统调用函数配宿主桩；9 个审计专用用例重现下述错误，另执行了引入模块中的 49 个原有用例，共 58 个通过。**这些用例断言的是目前的错误行为，全部通过意味着问题得到复现，不能当作修复验证。** W^X 用例验证的是源码中的标志位表达式，不是来宾中完整攻击链。

本地日志与报告：`build/audit-2026-11-14/`。其中 target、staged rootfs、镜像、截图和日志均为可重建审计产物，不应纳入源码提交。

## 2. 应先处理的安全与数据问题

优先级：P0 为安全边界失效或核心数据损坏；P1 为重要功能错误；P2 为兼容性、维护和体验问题。

### B01 · P0：普通 VFS 没有应用文件权限隔离

位置：`kunxcore/src/vfs.rs:649` 的 open_impl、`:768` 的 write、`:888` 的 unlink、`:906` 的 rename。

普通 SFS / devfs 路径没有基于 current_uid、文件 owner/mode 的访问检查。`/kos/` 路由有传 UID 的独立实现，不能把其检查推广到整个 VFS。launcher 调用了 setuid，也无法因此隔离普通文件路径。

后果：应用 UID 本身不能阻止读取其他应用数据、改配置或打开高权限设备。涉及具体系统文件覆盖的完整来宾攻击链本轮未执行。应补齐 inode ownership/mode、目录搜索权限和设备访问策略，且对 open/unlink/rename/mkdir 等统一执行。

### B02 · P0：修改页权限保留执行位，可产生 RWX

位置：`kunxcore/src/process.rs:2268`、`:2285`，`kunxcore/src/vm.rs:257`、`:260`。

`FLAG_USER_CODE` 包含 X，而 `AP_MASK=U|R|W` 不含 X。`(old_flags & !AP_MASK & !FLAG_COW) | AP_USER_RW` 会把只读可执行页改成可写可执行。系统调用层拒绝传入 PROT_EXEC，并不能消除旧的 X。

证据：审计复现从当前源码提取常量和实际表达式，确认结果同时含 W 和 X。应在权限转换中重建完整 R/W/X，检查最终权限组合，而非只检查请求参数。

### B03 · P0：写用户缓冲时主动把真正只读页改成可写

位置：`kunxcore/src/process.rs:2119`，尤其 `:2136`；`kunxcore/src/linux_compat.rs:369`。

ensure_user_range_writable 把 COW 与普通 AP_USER_RO 页都交给 mprotect_page(...RW)。后者包括 ELF text/rodata；这使 read、clock_gettime 等输出缓冲检查具有“替调用者解除只读”的副作用，并可联动 B02。COW 可写副本与真正只读权限需要分别处理，真正只读输出缓冲应拒绝。

### B04 · P0：同步 blob 每写一条就清掉上一条

位置：`shared/sync/app_platform.rs:49`、`sync_tests/src/host.rs:49`、`shared/sync/store.rs:247`。

write_file_at 每次都 O_TRUNC / truncate(true)，Store::save 又逐条调用它。复现：先在偏移 0 写 ABC，再偏移 3 写 DEF，得到 `00 00 00 44 45 46`，而非 ABCDEF。影响联系人/日历同步持久化。

接口注释 `shared/sync/fs.rs:15` 本身相互矛盾：既要求每次截断，又要求后续保留旧内容。应先创建/截断一次，再按偏移连续写；mock、HostFs 与 SysFs 必须遵守同一契约。

### B05 · P0：同步保存把旧 blob 偏移写进磁盘索引

位置：`shared/sync/store.rs:220` 到 `:274`。

save 先序列化旧 boff 并写索引，再压实 blob，最后只在内存重算 boff，没有把新索引写回。即使修复 B04，本问题仍独立存在。

证据：用保持偏移写语义的 MemFs，创建 OLD、保存、更新为 NEW!、再保存；当前内存读 NEW!，新 Store 重新 load 后读不到 NEW!。应先计算最终布局，再事务性提交对应索引与 blob，保证重启一致性。

### B06 · P1：旧 `.pkg` 所谓签名不验证发布者身份

位置：`kunxcore/src/pkg.rs:95`、`:107`、`:221`；可达入口 `kunxcore/src/shell.rs:1087`。

sign 是无密钥的四组 FNV-1a，任何人都能修改 ELF 后重算校验。审计用例实际构建包、修改内容、重算 sign 后 verify 重新通过。**这是旧 NOVAPKG1 路径；kunxpkg 的另一套证书/Ed25519 机制确实存在，不能说整个项目都没有验签。** 应停用旧安装格式或统一到已认证的包验证链。

### B07 · P1：磁盘“加密”是固定公开密钥的循环 XOR

位置：`kunxcore/src/blkdev.rs:461` 到 `:482`。

密钥硬编码；且 `(i + sector * 512) % 16 == i % 16`，扇区号事实上不会改变密钥流。它是演示性混淆，无法提供真实静态数据保密与完整性。只有启用该包装的分区受此实现影响，本轮未证明所有 /data 部署都经过它。

### B08 · P1：同步账户秘密为设备 ID 派生的可逆混淆

位置：`shared/sync/accounts.rs:203` 到 `:220`；`apps/contacts/src/main.rs:82`、calendar 对应设备密钥声明。

FNV 派生 32 位 key，再通过确定性 XOR 流存储；没有认证标签和硬件密钥保护。应使用受保护的随机设备密钥及经审查的加密实现。此处与 wireless/wifi_config 中另有的保护机制应分开评估。

### B09 · P1：锁屏密码哈希为快速无盐 FNV-1a

位置：`apps/launcher/src/main.rs:1821`，与 OOBE 的保存格式配套。

可读取 security_hash 后，短 PIN 很容易离线枚举。应迁移到随机盐与适合口令的 KDF，并结合文件权限、尝试节流与可信密钥存储；单纯换成长一些的快速哈希不足以解决问题。

### B10 · P1：DAV 同步认证走明文 TCP

位置：`shared/sync/app_platform.rs:92` 的 SysNet、`shared/sync/http.rs:75`、`shared/sync/dav.rs:269`。

使用 Basic base64(user:pass) 经普通 TCP 发送，默认端口 80；该同步路径没有复用浏览器 TLS。连接非可信网络时可暴露凭据。应接入 TLS 与证书/主机名验证，并明确协议与端口配置。

## 3. 已证实的 ABI、构建与逻辑错误

| ID / 级别 | 位置 | 触发与结果 | 验证 |
| --- | --- | --- | --- |
| B11 / P1 | linux_compat.rs:630、:3079 | clock_nanosleep 直接调用 nanosleep；前者 a0 是 clockid，后者当 timespec 指针读。CLOCK_MONOTONIC=1 变成地址 1，返回 EFAULT | 宿主抽取当前函数复现 |
| B12 / P1 | linux_compat.rs:657、:2448 | execveat 与 execve 共用解析；execveat 的 dirfd 被当路径，后续参数也错位 | 分派和函数源码证实，未在来宾触发 |
| B13 / P1 | linux_compat.rs:644、:2930 | pselect6 传入 tmo_reg=4，函数直接读 x[4]，实际 a4 在 x[14]；读的是 tp 而非 timeout | 零超时输入仍记录 50ms 睡眠，宿主复现 |
| B14 / P1 | linux_compat.rs:2618；vm.rs:110 | MAP_FIXED_NOREPLACE 用“整个范围都已映射”判断冲突；部分重叠返回 false，随后 unmap_user_range 删除旧映射。LAZY 预留范围也需要独立占用判断 | 两函数语义与调用顺序证实 |
| B15 / P1 | linux_compat.rs:2583；process.rs:1972 | 匿名 mmap 没按请求的只读 / PROT_NONE 保存权限，reserve_user_page 固定 FLAG_USER_DATA；默认路径 mmap_anon 也如此 | 源码证实，需来宾访问权限回归 |
| B16 / P1 | linux_compat.rs:2686 | mprotect(PROT_NONE) 直接成功，既不撤销访问，也在地址范围检查之前返回 | 宿主复现高地址请求也返回 0 |
| B17 / P1 | linux_compat.rs:2927、:3007 | pselect/ppoll 无就绪时最多睡 50ms 就报超时；无限等待也提前返回。pselect 忽略 writefds/exceptfds；ppoll 不支持 POLLOUT、管道/socket 就绪 | 源码证实；不可按注释“提前超时合法”接受 |
| B18 / P2 | linux_compat.rs:3011、:3027 | ppoll nfds=0 不执行 timeout，不能当延时器；fd<0 直接 continue，没有清零旧 revents | 两种行为均宿主复现 |
| B19 / P1 | kunxserv/rild/build.rs:11 | build script 无条件使用 std::os::unix::fs::PermissionsExt；Windows 宿主交叉编译即失败 | 实际 cargo check 的 E0433 / E0599 |
| B20 / P1 | Makefile:39、:226 等 | SHARED_SRC 缺 motion.rs / glass.rs / l10n.rs / shared/sync 等 include 输入；只改这些共享源码时，Make 可能判 ELF 已最新而不进入 cargo，发布旧二进制 | 对比显式依赖与 #[path]，属于增量构建缺陷 |
| B21 / P2 | linux_compat.rs:863 及相关 *at 调用 | 忽略 dirfd，不能保证 openat(dirfd,"relative") 相对目录打开；faccessat 仅检查存在，忽略请求的权限模式 | 源码证实 |
| B22 / P2 | vfs.rs:657、:693 | 已存在文件在 O_CREAT|O_EXCL 下仍走普通打开；没有 exclusive-create 契约 | 源码证实 |
| B23 / P2 | linux_compat.rs:424 | 路径遇缓冲区上限或中途不可读，返回已有前缀并补 NUL；调用者可能按截短路径操作，应分别返回 ENAMETOOLONG/EFAULT | 源码证实，完整路径误操作待来宾回归 |
| B24 / P2 | linux_compat.rs:1528、:640、:647、:655 | TCGETS、不支持的信号掩码/备用栈、mincore 等返回成功但未产生所需状态或输出；prctl/rlimit 也有广泛成功桩 | 源码证实，不能宣传完整 POSIX/Linux 兼容 |

补充：`apps/probe` 和 `apps/sleepy` 的当前 manifest 与 lockfile 不一致，使用 --locked 被阻止。它们是测试程序，**有用途**；这不是证明它们本身编译错误，也不是删除理由。

## 4. “屎山”具体在哪里，以及哪些东西可以清理

### 4.1 可维护性问题

1. **职责集中在巨型文件。** launcher/main.rs 9,428 行，shell.rs 5,066 行，oobe/main.rs 5,056 行，linux_compat.rs 3,857 行。launcher 同时管理锁屏、动画、文件夹、任务卡、缓存、输入和应用状态；共享可变状态让不同功能强耦合。宜按状态机和子系统逐步拆分，不能仅机械拆文件。
2. **全局 static mut 和宽泛 allow 掩盖约束。** 内核 main.rs 全局 allow(dead_code)，大量模块复制 syscall/常量/缓冲区代码；并发安全依赖单核、IRQ 屏蔽和调用顺序。需要逐项说明锁与所有权，不应仅去掉告警压制就宣称安全。
3. **多套相似实现漂移。** 内核 anim、shared/motion、控制中心 CcSpring、launcher 自己的运动逻辑并存；有不同曲线集合和打断策略。pkg 也存在旧格式与新证书格式双入口。应统一基础语义，保留必要的内核/用户态边界。
4. **应用用 #[path] 拼装共享源码。** 本身可以工作，但缺少版本化公共 SDK，也要求 Make 追踪全部间接源文件。当前 B20 是实际后果。
5. **用普通文件充当命令/响应信箱。** rild、audiod 等靠 /var/run 文件通信；已有不少并发修补，仍需身份认证、消息边界、背压、超时和故障恢复。应有统一 IPC 协议，而不是每个服务自行追加文件约定。
6. **成功桩让功能看起来比实际完整。** 不支持的必需语义应明确报错；能安全无操作的建议性调用才适合成功返回。尤其不能忽略会输出数据的调用。
7. **动画阻塞事件处理。** settings/pages.rs:126 入场循环约 425ms、:143 页面循环约 240ms，期间不轮询输入，难以取消、反向或立即响应系统导航。
8. **文档未随平台迁移。** README 仍写 EL1/PAN 和解析 inittab；init 实际硬编码拉服务。architecture.md 和 secure-boot.md 仍画已移除的 kunxboot、ARM EL3→EL1 链。安全库和 OTA 组件存在，不能据此声称当前 OpenSBI 的直接 -kernel 路径有文档所画的完整硬件信任链。

### 4.2 有编译器证据的清理候选

强制 dead_code 日志实际标出了 anim::spring、anim::page_transition、process::current_page_flags、部分 syscall_return_restore、blkdev::Partition、ui_framework 的 Ui/Widget 控件体系及若干 compositor 方法未被当前内核使用。

它们应分为：默认 QEMU 下可删的旧实现、仍计划接线的接口、其他 feature 专用实现、硬件寄存器/协议常量。**“默认配置未用”不等于“所有配置都可删”。** ui_framework 中字体初始化仍可能有独立用途，不宜整文件误删。完整列表见 dead-code.log。

建议移出正式文档入口或明确标记过期：`.audit_draft.md`、BUG_REPORT.md 的旧架构条目、animations-roadmap.md 互相矛盾的状态描述。先复核再归档，不能把旧漏洞列表全部算成现存漏洞。

不应视为废物：probe/sleepy/crashme/sfsrace 等测试应用、kunxsec、OTA/recovery、Pi4 的预留 HAL、third_party/fontdue 的 lazy 分支、源字体，以及 Make 明确部署到 rootfs 的字体/图标副本。Pi4 尚未适配到当前 RISC-V 内核，不代表其源代码完全没有研究用途。

可重建的 target、build 审计 stage、QEMU 截图、镜像和诊断依赖属于磁盘缓存。可以按需清理，不属于源码漏洞；本轮未删除用户文件。

### 4.3 CI 的实际盲点

`.github/workflows/ci.yml` 已有交叉构建和宿主测试，旧报告“CI 完全没有测试”已经不成立。但当前清单没有运行 sync_tests、kunxsec 的宿主测试和 font_startup_host；text_host、sfs_host_test 的主程序断言也不会因为 cargo test 而自动执行。fmt 和 lint 为 continue-on-error，失败不会阻断。

新增检查应优先覆盖 B01–B05、系统调用 ABI，以及实际执行 SFS 主程序和桌面回归。不要只增加测试数量而继续依赖与真实平台语义不同的 mock。

## 5. 动画：已有、没接好、尚缺

### 5.1 确实已有的内容

- shared/motion 具备缓动、弹簧与 PullRefresh；内核 anim 有窗口属性驱动。
- launcher 有图标反馈、启动/返回、翻页、最近任务、文件夹、锁屏、壁纸淡化、Toast、音量 HUD 等路径；OOBE 有自己的动画循环。
- settings 有页面转场和列表错峰；files/store/sms 有应用侧 motion 接线；gallery 有缩略图与大图 zoom_open/zoom_close。SMS 接了 PullRefresh。
- launcher/main.rs:879 已有通话胶囊横幅。因此“完全没有任何胶囊/流体云”不准确。
- 本次运行验证的是桌面与设置启动/返回等回归，其他动画主要依据源码确认，未逐一肉眼验收。

### 5.2 缺项与不完整项

| 优先级 | 缺项 | 证据与建议 |
| --- | --- | --- |
| P0 | 全场景可打断、继承速度 | 文件夹 opening 时 close 直接把 pos 设为 1、vel 清零（main.rs:6114）；关闭中不能重新 open；会跳变。锁屏转场期间 handle_lock_input 直接消费输入（:7055）。控制中心 shade_drag_allowed 只允许 Closed 接管（control_center.rs:209） |
| P0 | 非阻塞的应用动画调度 | settings 的转场和首屏入场用阻塞循环，图像能动但输入要等。应并入事件循环并支持取消 |
| P0 | 减少动态效果/动画时长统一设置 | 当前检索未找到覆盖 launcher、OOBE、应用的统一 reduced-motion 或 animation-scale 路径；辅助功能配置只有部分视觉选项。应支持关闭大幅缩放与视差 |
| P1 | 更多应用的页面与控件运动 | browser、contacts、calendar、notes、phone、camera、music、mail、map、weather 等未接入 shared/motion；仍需逐页检查自定义动态绘制，不能仅据 import 缺失断言它们完全零动画。优先补二级页、弹窗、开关、列表增删 |
| P1 | 通用共享元素转场 | gallery 有专门实现；未找到供 store 卡片→详情、settings 列表→子页共用的组件。普通整页滑动并不等同元素连续变形 |
| P1 | 通用下拉刷新 | PullRefresh 存在，但检索到的应用消费方是 sms；mail/store 等仍未接入通用组件 |
| P1 | AOD→锁屏、锁屏壁纸视差 | 未找到完整 AOD 状态、绘制与唤醒转场接线；桌面已有翻页视差不等同锁屏壁纸视差。QEMU 平台没有真实低功耗挂起后端，需要先处理状态与能力边界 |
| P1 | 通用实时活动胶囊 | 当前通话横幅已有；音乐、计时器、下载/充电等多来源状态统一仲裁、展开卡片和双轴形变，未见完整通用实现 |
| P1 | 呈现统计与一致性 | 未找到路线图提出的 /proc/anim_stat。需要 measured frame time、P50/P95/P99、超预算帧和输入到显示延迟，不能拿配置的 Hz 当实测流畅度 |
| P2 | 渲染能力与视觉打磨 | 当前主路径是 CPU 软件帧缓冲与合成，缺少完整 GPU 合成/硬件同步呈现能力。先做脏区、缓存与观测，再选择性增加模糊/投影，避免所有效果都整屏重画 |

`docs/animations-roadmap.md` 一处说应用侧全部零动画，后文又写多个应用已迁移；同一文档还引用已失效的旧平台文件。应改成有测试证据的状态表，不宜直接照着剩余清单重复开发。

## 6. 和 Linux 的区别

首先应区分：**Linux 是内核，常见 Linux 发行版是内核加用户空间；KunxOS 仓库包含自研内核、服务与桌面。** 比较桌面动效时，对象应是具体桌面环境/合成器，Linux 内核本身没有统一动画风格。

| 方面 | 当前 KunxOS | Linux 内核 / 常见发行版 |
| --- | --- | --- |
| 内核来源 | 自研 Rust / 少量汇编等，主目标 RISC-V QEMU virt | Linux 内核及其驱动、子系统生态；发行版搭配各自用户空间 |
| 程序接口 | 支持一部分 asm-generic Linux syscall，存在上述 ABI 与成功桩差异 | Linux ABI 本身；并非所有发行版的软件包都可跨架构/发行版直接运行 |
| ELF/应用生态 | 自有静态 ELF 应用、部分 Linux 兼容路径 | 广泛的 libc、动态加载器、工具链和发行版软件包 |
| 文件与权限 | SFS/FAT32/KOS 等；普通 VFS 的 owner/mode 强制访问缺失 | 成熟的凭据/DAC，按配置可有 ACL、LSM、SELinux/AppArmor 等 |
| 资源规模 | 当前 MAX_PROC=24、全局 MAX_OPEN=256 等固定表 | 通常更可伸缩且有资源限制、命名空间、cgroup 等体系；具体取决于配置 |
| 硬件支持 | 当前重点是模拟平台；Wi-Fi、相机、传感器等含软件模拟后端 | 广泛真实设备驱动，具体设备仍取决于内核版本与厂家支持 |
| 图形 | 自研软件绘制/合成与帧缓冲 | 常见桌面配 DRM/KMS、Mesa、Wayland/X11 与 GPU 合成 |
| 功耗 | 有策略和模拟状态；QEMU 后端 sleep 无真实挂起 | 多平台 CPU idle/freq、挂起、热管理与设备电源管理 |
| 工程成熟度 | 适合研究、演示与继续开发，关键安全边界待补 | 多年生产使用与生态维护；也持续存在并修复漏洞 |

当前最大的差距是接口契约、隔离和硬件/生态完整度，不能用代码体量或动画外观替代这些指标。

## 7. 和 Android 17 的区别

**Android 也使用 Linux 内核；KunxOS 目前不是 Android 分支或 Android 应用运行环境。** 具有类似桌面、手势、UID、HAL 命名，并不自动获得 Android 兼容性。

本轮尝试查询 Android 官方资料时，搜索连接器启动失败，直接访问开发者站点也超时。因此下面比较的是 Android 平台长期架构与当前源码差距，**没有核实 Android 17 独有的新特性、当前发布阶段或 API 级别**，不把推测写成版本事实。

| 能力 | 当前 KunxOS | Android 平台的对应体系（不作为 17 专属特性宣称） |
| --- | --- | --- |
| 应用运行 | 自有 Rust/ELF 程序 | Android Framework、ART、Java/Kotlin/NDK 与 APK 生态 |
| 通信与服务 | 自有 syscall、socket、文件信箱等 | Binder、系统服务与平台接口契约 |
| UI/图形 | 自绘控件、软件合成，运动主要在 launcher | 应用 UI 框架、窗口系统、SurfaceFlinger 与硬件图形栈 |
| 应用隔离 | setuid/部分进程权限；普通 VFS 未完成隔离 | Linux UID 隔离配合 SELinux、权限体系和受控设备/服务访问 |
| 密钥与数据 | 部分自研加密验证；仍有 XOR 和 FNV 旧路径 | Keystore/KeyMint、设备能力对应的可信执行/密钥保护与平台存储加密体系 |
| 应用生命周期 | 已有 lifd/最近任务/内存压力等实现，但接口与覆盖有限 | 框架管理的组件、进程优先级、后台执行限制与应用恢复契约 |
| 设备能力 | 大量模拟能力，真实相机/基带/蓝牙/电源适配有限 | 厂商 HAL、驱动与设备认证生态；设备具体能力由实现决定 |
| 兼容性验证 | 本仓单元/脚本与部分 QEMU 回归 | CTS/VTS 等平台兼容性测试体系 |
| 动画差距 | 已有曲线与转场，但输入接管、统一框架和呈现观测不足 | 动画与窗口、手势、应用生命周期和图形提交体系协同；厂商界面还有额外实现 |

Android 17 应用不能因为 syscall 名称或画面相似就直接在当前 KunxOS 上运行。要兼容 APK，需要处理运行时、Framework、Binder、包/权限/资源系统、图形与 native ABI 等完整依赖，工作量远大于增加启动动画。

官方核验入口（本轮未成功读取，作为后续查证路径，不是本轮版本特性引用）：Android Developers 的 Android 17 版本资料、AOSP Architecture、Application Sandbox、SELinux、CTS/VTS 文档。

## 8. 建议实施顺序

1. 先修 B04/B05 数据损坏、B01 文件隔离、B02/B03 页权限；同时给真实 HostFs/SysFs 与页权限增加回归。
2. 禁用或迁移旧 FNV 包安装路径，给磁盘/账户/锁屏的演示保护明确降级标识并替换；DAV 接 TLS。
3. 修 clock_nanosleep / execveat / pselect / ppoll / mmap 的实际 ABI 与权限契约，避免无条件成功桩。
4. 修 Make 间接依赖、rild 宿主兼容，补进关键宿主测试与实际运行型回归；为过期文档建立状态表。
5. 再做运动框架统一、非阻塞可打断动画、减少动态效果设置和应用覆盖；以实际帧耗时及输入延迟评价结果。

不要先大规模删库或堆特效。先把会损坏数据、破坏隔离和发布旧代码的问题消掉，再按编译器证据逐模块减少冗余。
