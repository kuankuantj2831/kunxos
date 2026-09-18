# Changelog

本文档记录 KunxOS 各版本的所有重要变更，遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/) 约定。

## [Unreleased]

### 2026-09-18 深夜续轮：GUI 应用全挂的根因修复（tty_read 无视 O_NONBLOCK）

> 第十九轮为 musl fgets 把 /dev/tty read 改成阻塞语义时，yield 循环实现
> 无视 O_NONBLOCK——所有 GUI 应用主循环的 `poll_tty_esc()` 读阻塞 fd 0
> 即被拖进内核 yield 死循环：应用永远回不到用户态，不渲染/不读输入/
> 不上屏（14 应用全挂 + 纯色截图 + 前台输入死路的完整根因）。双层修复：
> 内核 tty_read 尊重 O_NONBLOCK（无数据返回 -EAGAIN）；kit 的
> poll_tty_esc 改用独立非阻塞 tty fd（fd 0 保持阻塞给 musl）。修复后
> launcher_test 全链路 PASS（启动→就绪→应用内返回退出→launcher 恢复
> 输入），此前 100% 挂死。**apps_regression 14/14 全 PASS（fail=0）**，
> 截图 15/19 有真实 UI 内容（修复前 0/14 全纯色）——带真实应用启动的
> 首次全绿（历史「通过」全部是 gate 写失败的退化形态）。calculator/
> settings/stub/store 各自的 `poll_key()` 直读 fd 0 同族修复。详见
> `BUG_REPORT.md` 第二十六节根因修复段。

### 2026-09-18 GUI 前台路径调查 + 测试修正（存档）

> inode 512 扩容让启动门控首次真正写成功、应用真正启动，暴露未提交生命
> 周期重设计的前台输入路径缺陷（应用前台期 launcher 设计性不读输入、
> 应用侧只读到 1 次事件即不再轮询）+ launcher_test 断言过期（历史通过
> 均为 gate 写失败的退化形态）。测试已按真实生命周期流程重写（偏移锚点
> 防陈旧匹配、应用内返回→退出→桌面恢复验证）。完整证据链见
> `BUG_REPORT.md` 第二十六节。非 GUI 回归全绿。

### 2026-09-18 第四波修复轮：FAT32 互操作 + TCP MTU + rild 短信链路 → 20 项修复

> **FAT32 目录项首簇 HI/LO 与规范相反**（内核+mkdisk 同向写反、自产镜像
> 自洽掩盖——标准 mkfs.vfat 卷挂载全部读空），全部读写点对调 + disk.img
> 重建 + run-disk 引导验证。TCP 发送段上限收紧 1460（此前可组 2048 字节
> 超长帧，真实网卡黑洞）。rild 短信链路两高危：**AT 行缓冲 160 截断 PDU
> （正文 >50 字符的短信永远收不到）**→ 448；命令等待期 URC 吞失（发短信
> 窗口内的新短信/来电不可见）→ 已知前缀仍派发。alarmd 每周闹钟按 UTC 求
> 周几（东八区 08:00 前的起床闹钟整体晚一天）→ 本地日历日。camerad 无
> 后端 panic ×9 处、录像容器交错、audiod 陈旧块无限重播、logd 突发丢日志
> 等。net_virtio SMP 跨核竞态移交 SMP 锁纪律轮。详见 `BUG_REPORT.md`
> 第二十五轮。

### 2026-09-18 第三波修复轮：内核核心 + shared 全量读码 → 17 项修复（含新动画代码自纠）

> 内核：shm_map/shm_close 引用计数 IRQ 竞态（UAF）、dup 失败回滚对活进程
> 误调 shm_close_index（UAF，newfd>=FD_MAX 稳定触发）→ 新增
> shm_drop_one_ref、信号位图 RMW 竞态（SIGKILL 可丢）、now_us 频率换算
> 截断（每秒慢 0.8%，perf/deadline/超时全部失真）。shared：sync 墓碑
> 残留（远端重建资源不可见 + 周期性误删服务器数据）与 apply_deleted
> 服务器优先不落删除、sms 标题栏负整除误开会话、text_engine BiDi
> 字节/字符错位、l10n 阿拉伯语复数 &&/|| 链解析、emoji CBLC 越界 panic、
> jpeg 尺寸回绕、cursor_overlay 首帧黑块、gallery 不可见行命中。
> **本会话新动画代码自纠 3 条**：PullRefresh 弹簧被 frame() 的 last 覆盖
> 饿死 16 倍速、ate 残留吞后续 tap、glass 回零 tween 每帧重启。
> 另：P0-2 files 页面转场、P0-4 音量 HUD（音量键此前在输入分发中被
> 丢弃）落地。详见 `BUG_REPORT.md` 第二十四轮。

### 2026-09-18 第二波修复轮：TLS 安全（证书链 CA 校验 + critical 扩展解析根因）+ 信箱迁移收官 + 19 项修复

> TLS 栈深挖发现 **证书链无 CA/basicConstraints 校验**（持有效叶子证书可
> MITM 冒充任意域名）与**critical 扩展解析拿错 tag**（basicConstraints 恒
> 为 critical、被静默跳过——SAN 非 critical 故从未暴露）两个联合缺陷；
> 修复后对 www.baidu.com 真网完整握手 + HTTP 200 实测通过。畸形告警记录
> 远程 panic、close_notify 长度、HelloRequest 哈希、DER 长端编码一并修复。
> 信箱 rename 认领迁移收官：camerad/sensord/positiond + lc_notify_local
> 原子发布（第二十轮的 /var/run 部分补齐）。terminal UTF-8 组装器丢 CJK
> （凑 4 字节与 3 字节序列错位）+ 历史环乱序 + CSI 短写；calendar 跨零点
> DTEND、camerad id>999 硬拒、recovery reboot 系统调用号（169→142）、
> MPU 传感器大端、kunxboot dtb 边界端点。详见 `BUG_REPORT.md` 第二十三轮。

### 2026-09-18 修复轮：inode 容量根因 + 四路审查 19 项修复 + 4 组动画

> sfsrace BASELINE FAIL 根因为 **SFS inode 容量耗尽**（rootfs 涨到 196
> 文件后，sfsrace + 守护进程信箱数秒烧完剩余 60 个 inode，全系统 create
> 失败——第二十轮「/var/run 创建即失败」的真正答案）。`MAX_INODES`
> 256 → 512（内核 + mkfs 同步，`parse_sb` 加 SB_NUM_INODES 护栏拒绝旧
> 镜像，`rm disk.img && make disk` 重建）。另经 4 路并行审查修复 19 项：
> O_APPEND 死代码（追加链路整条不可达）、ppoll ABI 错位（musl poll 恒无
> 就绪）、fb_present BUSY 死属主卡死整机画面、wait 重试丢唤醒、browser
> HTTPS fd 泄漏（16 槽 socket 表十几次耗尽）、networkd 管道永久失效、
> kunxpkg 菱形依赖约束短路、launcher 占位页守卫恒假等。动画：P1-6 下拉
> 刷新（motion.rs PullRefresh + sms）、P1-2 gallery 一镜到底（可打断
> 续跑）、P0-2 sms 页面转场、P2-2 光随指动（glass 光源偏移 + 全应用输入
> 自动喂点）。详见 `BUG_REPORT.md` 第二十二轮。验证：make 0 警告、
> sync 57/57、wireless 105/105、sfsrace PASS。

### 2026-09-16 SMP/PSCI 打通轮：副核唤醒 + 每核状态 + 三项 SMP 债务修复

> 只做「SMP/PSCI 打通或显式降级」：结论为**打通至双核上线可用、压测
> 仍实验性**。详见 `BUG_REPORT.md` 第二十一轮。

- **PSCI CPU_ON 打通**：旧结论「-kernel 模式无 PSCI」系 conduit 误用
  （`smc #0` 在 secure=off 下 UNDEF）；QEMU virt 的 DTB `/psci` 报
  `method="hvc"`，QEMU 自身拦截 hvc 分派 PSCI。`smp_init` 改 DTB 检测
  conduit/函数号 + `PSCI_VERSION` 探测（无 PSCI 显式单核降级）+ 逐核
  CPU_ON。实测 `-smp 2` 两核在线、cpu1 承载用户任务（SVC/缺页实证）。
- **副核上线四阻塞点修复**：GICC 每核偏移错误（GICv2 banked，去
  `cpu_id*0x20000`）；`SECONDARY_STACKS` 1 字节对齐致 SP 歪（Alignment
  fault 风暴）→ `#[repr(align(16))]` + 入口兜底对齐；栈区误落 .rodata
  被恒等映射标只读（Permission fault）→ `static mut` 落 .bss；每核
  MAIR/TCR/TTBR0/SCTLR/PAN 从未在副核初始化（用户任务首调即 SIGILL）
  → 新增 `mmu::init_secondary()`。
- **SMP 债务 ×3**：PROCESSES 槽位 `PROC_CLAIM` 原子认领（alloc/reap/
  reap_forced 全路径）；execve 跨核 kill_req 窗口（kill_siblings_of
  静默等待他核兑现 + pick_next 不复活 kill_req 任务）；fs_lock/unlock
  持有身份改 TID 单令牌（迁移后解锁不再丢嵌套深度/误清存活持有者）。
- **定位与文档**：`-smp 2` 为实验性（sfsrace 双核压测偶发任务表踩踏，
  指向调度器 static mut 无锁并发面，三债之外的遗留工程）；默认回归与
  发布仍单核。`make run-smp` 更新为真 SMP + verbose，DEBUGGING.md 同步。
- **构建系统**：`test-sfsrace` 间歇性 FAIL（mkfs 与上次 cargo 编译
  同秒 → mtime 粒度内误判 Fresh → 测试标记不重嵌内核）——mkfs 后
  `touch $(RAMDISK)` 强制重编。
- 验证：单核 boot / `make test-sfsrace` / `make power-test` 21/21 全绿；
  `-smp 2` 上线稳定、sfsrace 一次全绿、power 20~21/21（时序敏感项）。

### 2026-09-16 通宵轮②：对话框统一轮回归清剿 + lifd 族信箱原子化 + 5 组动画

> 与同期 TLS/NetSurf 轮并行作业（本轮边界：launcher / 应用侧 / lifd 族
> 信箱）。方法：3 路并行审查代理（未提交 diff 回归 F1~F11 / 信箱竞态盘点
> / clock-notes-cloud 方案）+ 主会话实施。详见 `BUG_REPORT.md` 第二十轮。

- **对话框统一轮回归修复（审查发现，全部未提交改动的自身缺陷）**：
  blit_icon 快路径绕过 TILE_MODE（启动转场瓦片错图标 + 屏幕左上角污染）；
  小组件玻璃缓存键补几何（布局变化后不再恢复陈旧玻璃）；「桌面设置」卡
  直接点确定不再把壁纸序号误当网格选择（独立暂存 SHEET_GRID）；Toast
  退场残影补一帧清理；文件夹压暗缓存跨分钟不再冻结状态栏时钟；浏览器
  chunked 解码 size 回绕溢出（远程可触发 panic）改 checked 加法；编辑
  选择卡白卡入场淡入补齐（put_aa 乘 PIX_ALPHA）；对话框复选框角弧描边
  缺口；鼠标长按补控制中心判定。
- **rild 短信存储原子重写**：已删短信「复活」（就地重写不 ftruncate、
  陈尾被当记录）+ 掉电半行损坏 → rename 备份 + O_TRUNC 全新写 + 启动
  恢复（`recover_store`，丢的只是一次删除/标记，记录本体无损）。
- **clock / notes 逐级返回**：闹钟编辑页与笔记编辑器的返回/ESC 不再直接
  退出整个应用（编辑中误触即丢），先回列表、根页才退出（对齐
  calendar/contacts）。
- **cloud 长按解锁改真长按**：原实现实为「间隔 2-3s 双击」（只在抬起帧
  判定）—— 改每帧检测按住状态（≥2s 且未滑出按钮即解锁，无需抬起），
  解锁后吞掉本次抬起防误触主界面按钮。
- **lifd 族信箱 read→clear 竞态清剿**（BUG_REPORT 十八记录的系统性缺陷，
  /data/life 部分）：应用命令改每消息一文件（`cmd_<pid>_<seq>` 临时名
  rename 发布，lifd 读方零改动）；lifd 事件改 `.new` + rename 发布、应用
  改 rename 取走（取走与读取之间新发布的事件不再被 unlink 吞掉）；
  pushd / launcher 同步迁移；`lc_init` 就绪探测改目录。
- **动画（详见 `docs/animations.md` §3.20~§3.23）**：Toast 长文案折行 +
  保持期按行数延长 + 退场上滑 8px；最近任务伪 3D 纵深（P2-3）；锁屏
  分钟切换双时刻错位入场 + 解锁时钟先扩后散（P1-4 部分）；settings
  主列表 / store 卡片首屏入场错峰（P0-6）。
- 验证：`make` exit 0；宿主 sync_tests **57/57**、wireless **105/105**；
  lifecycle-core **38/38**（隔离 worktree 验证 —— 隔离对照揪出并修复了
  迁移自身的两个回归：lifd 在 getdents 迭代中同目录 rename 发布使游标
  错位跳过命令文件（改两段式扫描）；publish 的 rename 在目标被 `cat`
  打开时失败丢事件 + `.new` 泄漏 inode（重试 + 就地覆写回退）。共享树
  复测被同期并行会话的内核在制回归（启动期任意路径文件创建失败）阻塞，
  定位记录见 BUG_REPORT 二十「附」）。

### 2026-09-12～16 浏览器 TLS 1.2 + NetSurf 移植轮（musl 静态 Linux 程序支持）

> 双主线：① 浏览器内置 TLS 1.2 客户端栈（此前 HTTPS 完全不可用，重定向到
> HTTPS 的站点一律打不开）；② NetSurf 全套（libparserutils → libdom/libcss →
> framebuffer 前端）以 musl 静态交叉编译方式移植上机 —— 为此内核补齐一整批
> Linux/musl 兼容能力。验证基线全部通过，详见各条与 BUG_REPORT 十九。

- **TLS 1.2 客户端栈 `apps/browser/src/tls/`（全新）**：
  - `mod.rs` TLS 1.2 状态机（ECDHE_RSA_AES128_GCM_SHA256 密码套件、SNI、
    会话密钥推导 + 加密通道读写）；`aesgcm.rs` AES-128-GCM；`rsa.rs`
    RSASSA-PKCS1-v1_5 验签；`p256.rs` NIST P-256 点乘/ECDH（含自测）；
    `x509.rs` 证书链解析与校验（签名/有效期/主机名，ECDSA 链显式判不可信）；
    `roots.rs` 内置根证书库（`tools/gen_tls_roots.py` 生成）；哈希复用 kunxsec
    的 SHA-256（browser 新增依赖）。
  - `net.rs` 连接抽象 `Wire`（明文/TLS 统一读写），`https://` 直达；握手失败
    转可读错误（「TLS 握手失败：…」）。
  - 双端自测（共用同一份 tls/ 源码，`#[path]` 引用）：`apps/tlstest` 来宾侧
    （QEMU slirp 真网对 www.baidu.com:443 全握手 + 取页，`[tls-selftest] PASS`），
    `apps/browser/hosttest` 宿主侧（std TcpStream）；`tests/scripts/tlstest.py`
    驱动 + Makefile `tlstest` 目标。
- **NetSurf 移植**：
  - `vendor/netsurf`：netsurf + libparserutils/libwapcaplet/libhubbub/libcss/
    libdom/libnslog/libnsgif/libnsbmp/libnsfb 全家桶源码；
    `vendor/netsurf/build-kunxos.sh` 以 musl-gcc 静态交叉编译
    （`tools/musl-cross` 包装器）。
  - 移植层：自写 libnsfb **kunxos surface 后端**（`libnsfb/src/surface/kunxos.c`，
    渲染直出 /dev/fb0）+ monkey 前端 `argv[1]` 直开 URL 的无 stdin 冒烟路径。
  - 产物入 rootfs：`/bin/netsurf-fb`（2.4MB）、`/bin/nsmonkey`（2.3MB）、
    `/usr/share/netsurf` 资源、`/etc/mime.types`、`/etc/test.html`。
  - nsmonkey 无头冒烟 PASS：WINDOW NEW → file:// 取页 → HTML/CSS 解析 →
    排版（UPDATE_EXTENT/TITLE 事件齐全）全链路在本内核上跑通。
- **内核 musl/Linux 兼容（本轮内核侧主体）**：
  - `elf.rs`：Linux **auxv** 完整构造（AT_PHDR/PHENT/PHNUM/PAGESZ/ENTRY/UID/
    EUID/GID/EGID/HWCAP/CLKTCK/SECURE/RANDOM/EXECFN）—— musl 静态启动依赖
    AT_PHDR 定位程序头表，缺失即启动即死。
  - `bionic_shim.rs`：`set_tid_address` + **CLONE_CHILD_CLEARTID**（线程退出
    内核清零该用户字并 futex 唤醒 join 方 —— musl pthread_join 硬依赖）。
  - `linux_compat.rs`：**pselect6 + ppoll**（musl 的 select()/poll() 事件循环
    依赖；ppoll 此前缺失使 nsmonkey 的 select 轮询永不读输入——本轮验证期
    发现并补齐）；`rt_sigprocmask` 成功桩（musl 启动路径 __block_all_sigs）；
    **write/pwrite >4KB 短写修复**（此前 `count.min(4096)` 静默截断，朴素
    调用方丢数据且不自知）；writev 零长 iov 跳过（否则 musl 无缓冲流死循环）；
    未实现 syscall 串口诊断（`[syscall-unimpl] nr=`，移植定位用）。
  - `devfs.rs`：UART 读**阻塞语义**（POSIX：无数据阻塞而非返回 0=EOF ——
    返回 0 会让 nsmonkey 等交互程序 fgets 即退）。
  - `socket.rs`：TCP **动态接收窗口通告**（固定窗口让对端持续超发、缓冲满后
    丢段只能等 RTO 重传，吞吐骤降）+ 取走数据后立即 ACK 重开窗口 + 对端 FIN
    重传补 ACK（此前不回，对端重传到放弃、拆除卡在 CLOSE_WAIT）。
  - `vm.rs`：用户栈 1MB → **8MB**（NetSurf 深递归），ASLR 窗口与隔离带同步。
  - `main.rs`：页故障解决顺序重构（DFSC 编码掩码归一：先试 LAZY 预留页落地、
    再 COW、均失败才 SIGSEGV —— mmap 匿名区落在继承的 2MB 块映射内时此前
    误报 L2 权限错误致死）。
  - `process.rs`：wait **收尸防重入**（多线程父两线程同醒双收同一僵尸 → 双重
    free_process_tables、页二次归还分配器）；pid 复用时清理 ELF 运行时信息与
    信号 handler 表（陈旧 ASLR 栈基址/brk、残留 SIG handler 会被新进程继承）。
  - `net_virtio.rs`：virtio-mmio IRQ **无条件 ACK 全部 cause**（纯 config-change
    此前永不 ACK → 电平中断被 GIC 反复重投递 → 风暴护栏误禁整块网卡）。
  - `allocator.rs`：页引用计数 RMW 全部 IRQ 屏蔽（与 exit 路径 free 交错丢一次
    递增/递减 → 计数永不归零 = 4KB 页泄漏）。
  - `vfs.rs`：readlink errno 对齐 Linux（不存在→ENOENT、非符号链接→EINVAL，
    musl realpath 逐组件调用且只容忍 EINVAL）；`readdir_back`（getdents64
    用户缓冲装不下「刚被消费」的条目时回退，修复目录枚举丢项）。
  - `kunxcore/build.rs`：恢复 `rerun-if-changed=ramdisk.img`（linker 行遮蔽
    dep-info 跟踪的根因补全，见 BUG_REPORT 十八·五）。
- **验证（2026-09-16）**：`make` 全量 exit 0 / 0 警告；宿主 sync_tests **57/57**、
  wireless **105/105**；QEMU：tlstest **PASS**（真实 HTTPS 全握手 + 取页）、
  power **21/21**、nsmonkey 冒烟 **PASS**。lifecycle-core 见 BUG_REPORT 十九
  与二十·附：共享树回归经并行会话定位为 lifd 信箱 rename 发布的扫描游标
  错位（已修复，隔离树 38/38）；叠加持久 disk.img inode 残留（已重建磁盘）
  与同机并发 QEMU 计时干扰，共享树终验待收敛后复跑。

### 2026-09-11 对话框统一（OriginOS）+ HyperOS 桌面编辑 + 桌面流畅度轮

> 四项主线：① 确认框/选择框全部居中（Pixel OS 语义）；② 对话框视觉统一为
> VIVO OriginOS 风格（白卡大圆角 + 发丝分割线 + 文字按钮）；③ 桌面卡顿
> 治理（重绘源头削减）；④ 长按桌面空白处进入编辑模式（小米澎湃 OS 3 语义）。

- **共享对话框组件 `shared/dialog.rs`（新）**：全系统统一的 OriginOS 风格
  居中模态 —— 白色卡片 + 超椭圆圆角 + 淡投影 + 细描边，标题加粗居中、
  正文居中灰字；按钮区 1px 上分割线 + 双按钮等宽 + 中间竖分割线
  （取消深灰 / 确认蓝 / 危险红）；选择行（单选圆点 / 多选方框）行间发丝
  分割线；内嵌输入框与进度条样式。经 `DialogUi` trait 与各应用自绘管线
  对接（含 `fill_row` 行混合热路径，遮罩不走逐像素虚调用），绘制与命中
  测试共用同一几何常量（永不漂移）。
- **files**：输入 / 删除确认 / 复制移动进度三个对话框迁移到共享组件
  （居中 + OriginOS 样式），命中测试改走 `dialog::hit_buttons`。
- **settings**：恢复出厂 / 还原设置 / 重启系统确认从「整页推入」改为
  **居中模态对话框**（遮罩 + 卡片覆盖当前页，不再携带页面转场；
  点外部 / Esc / 取消关闭，模态期间滚动与滑条拖拽屏蔽）。
- **oobe**：胁迫动作 / 外观两个选择步骤的散列卡片统一为单张 OriginOS
  风格选择列表卡（白底大圆角 + 行间分割线 + 右侧单选圈），行高与行距
  一致、整行可点。
- **launcher 桌面编辑（HyperOS 3）**：长按桌面**空白处**（此前仅应用图标）
  进入编辑模式，底部弹出磨砂**编辑条**（壁纸 / 小组件 / 桌面设置三个
  圆形按钮，上滑 + 淡入入场）；点选弹出 OriginOS 选择卡 ——
  壁纸（4 色单选，确认后交叉淡化切换并写回 desktop_meta）、
  小组件（5 类多选增删，位掩码持久化 /data/oobe/widgets_mask，
  编辑模式下点小组件即移除）、桌面设置（网格 4x6/5x6 单选即应用，
  替代原编辑 HUD 右上角按钮）。
- **桌面流畅度**：Toast 此前把「显示中」整体当动画源 —— 一条 2s Toast
  触发 2s × 60fps 整屏重绘（TCG 下每帧 50~150ms，桌面全程卡顿），
  现只在入/退场动画窗口（180/150ms）重绘；编辑模式抖动重绘 ~30fps
  节流；小组件毛玻璃按 (调色板， 主题, 类别) 五槽缓存（翻页/拖动/
  壁纸淡化期间自动退化为实时玻璃）；图标 blit 不透明段整段直写
  BACK_BUF 行切片；文件夹打开期间压暗桌面缓存（首帧 LUT 存档，
  后续帧整块 memcpy）；日历小组件事件文字 1s TTL 缓存（与天气/电池
  一致，动画期不再读盘）。
- **应用启动再提速**：标题字体（bold.ttf ~220KB）从 `font_init` 拆出为
  按需加载 —— 21 个应用中仅 settings/store 使用粗体，其余应用每次启动
  少解析一整份子集；launcher 启动不再加载 launcher-bold（桌面无粗体
  字形，粗体绘制自动回退常规）。字体子集随新文案重建（launcher-cn
  覆盖桌面编辑器新增词条）。
- **浏览器网络兼容**：实现 chunked 传输编码解码（此前直接报错，大量
  HTTP 服务器默认分块传输）；强制 HTTPS 的重定向给出明确错误文案
  （「该网站强制 HTTPS，浏览器 TLS 尚未启用」）；书签栏新增两个仍支持
  纯 HTTP 的真实站点（Example / CERN —— 主流站点明文端口普遍 302 强跳
  HTTPS，TLS 实现前可经此验证完整网络链路）。

### 2026-09-09 应用进出提速轮（对标 ColorOS 15 潮汐/极光引擎）

> 打开/退出应用的端到端延迟治理：转场动画本身（300ms）不是瓶颈，
> 慢在转场之外的串行等待 —— 每次启动的全量字体解析（秒级）、转场后的
> 整屏桌面重绘尾巴、动画结束处的阻塞轮询。

- **字体（启动最大耗时项）**：`make_fonts.py` 改为只提取真正上屏的
  「字符串字面量」（状态机跳过注释/代码；此前整文件扫描把全仓库中文
  注释的汉字都收进了 cn.ttf）并丢弃 fontdue 不读的表（name/hinting/
  GSUB/GPOS/post）—— cn.ttf 761KB→225KB、bold.ttf 760KB→224KB、
  launcher-cn 482KB→78KB、oobe-cn 407KB→82KB，每次应用启动的字体
  解析量降至 ~1/2.6，ramdisk 同步减负。
- **字体兜底**：`font_render` 新增缺字形懒加载（用户态开关）—— 子集
  不含运行期拼接字符（用户数据：联系人名/文件名等）时，首次栅格化
  一次性加载全量字体顶替并清字形缓存，字体路径永不渲染豆腐块；UI 类
  应用（文案全在子集内）永远不付这次加载。内核（ui_framework）共用
  模块但不开启，svc 文件读取路径在内核镜像里不会执行。
- **launcher 启动**：动画结束不再阻塞轮询应用首帧 60ms（恰在输入最
  敏感时刻卡主循环）—— 占位页立即兜底（ColorOS starting window 语义），
  应用首帧就绪后自然覆盖；子进程 execve 前自设 3s 一次性调度 deadline
  （syscall 414），加载/首帧以交互期限参与 CFS 竞争（潮汐引擎「启动
  线程优先」等价物）。
- **launcher 退出**：返回转场开始时就地预渲染桌面并存档（极光引擎
  「转场与目标帧并行绘制」）—— 缩回的应用帧叠加在桌面上（ColorOS 式
  「缩回图标、桌面在四周浮现」，去白闪），转场结束整体恢复直提，跳过
  转场后 50~150ms 的整屏 `draw_desktop_full` 尾巴；壁纸偏好同步从
  `return_animation_done` 挪到转场开始（离开关键路径）；启动转场白闪
  LUT 熔进背景拷贝单趟（转场后段每帧少一趟整屏扫描）；删除无调用方的
  阻塞 `reap_child`（死代码）。

### 2026-09-09 通宵修复轮（9 路并行审查 → 45+ 项修复）

> 4 路审查 2026-09-08 未提交改动（launcher 动画/手势、OOBE 光锥、settings
> 保留区、图标字体管线）+ 5 路子系统深挖（内核 syscall/进程、驱动/网络栈、
> pkg/boot/tools、系统服务、媒体应用），逐条复核后修复。详见
> `BUG_REPORT.md` 第十八轮。

- **内核**：PM 冻结/限速 syscall 补权限校验（任意应用可软冻结桌面）；
  reap() 防重入（多线程父进程双收同一僵尸 → 双重归还页表破坏分配器）+
  wait() 误报成功；sleep_until 不恢复 DAIF（调用方永久关中断运行）；
  write/pwrite >4KB 静默截断改循环写；pipe2 失败返回 fd=-1 仍报成功；
  getdents64 缓冲半满丢目录项（新增 vfs::readdir_back 回退）；reap 补清
  SIG_TABLES/ELF_PROCS（pid 复用后信号跳陈旧 handler）；PAGE_REFS 引用
  计数 RMW 关中断；帧统计冲刷快照关中断（打印仍可中断）；virtio-net
  无条件 ACK 全部 cause（config-change 风暴曾触发护栏禁网卡）；TCP 裸 FIN
  重传补 ACK；输入缓冲满丢整帧不劈帧。
- **launcher**：双线性采样左/上边缘外推（alpha 溢出/黑边/镂空）；连续启动
  打断的失败路径不再污染 Return 转场；resume 恢复转场零帧播完（spring 未
  赋 pos/target）；后台应用被外部杀死的僵尸 + lifd 槽泄漏（桌面期补
  waitpid 回收）；BG_RECENTS 7/8 字节窗口恒 false（最近任务手势返回不开
  面板）。
- **OOBE**：光锥扫光抹掉光标 overlay；恢复矩形削标题墨迹；逐帧全量栅格化
  （~17KB 字形超缓存，拆出复用）；完成页「上一步」照样进桌面。
- **settings**：保留区短拖变「抬起位置点击」误开列表项（12px slop）；按压
  反馈恢复绕过 LUT 留偏亮方块；底部保留区 buttons 模式纯属滚动死区（仅
  手势模式生效）；滑条/滚轮无变化不再写盘+整屏重绘。
- **服务**：lifd BTASK u64→u8 截断绕过白名单；wifid FORGET 只删文件（已
  忘网络仍自连、重 SAVE 不生效）；wifid getdents 缺解析守卫；alarmd 信箱
  256B 截断；rild 联系人只读 2KB；audiod best_seq 以 0 为基准（seq 越过
  2^31 后音频永久静默）；音频双槽 PCM 先写、seq 头最后提交（消 10ms 陈旧
  帧爆音）。
- **应用**：camera 预览帧路径差一（`&path[..n-1]` 切掉 'v'，取景器完全
  失效——一行修复）+ 导航退出不停预览流 + REQ_ID 位数越界；cloud 恢复
  路径 `..` 穿越、lock/wipe 无视本机开关、失败提示用陈旧 RESP；files
  del_tree 无深度上限；contacts 密码框触点漏第三字段；calculator 释放
  落空按钮卡在按下态。
- **pkg/store/boot/构建系统**：store SigV4 kDate 派生错误（date 拼进 HMAC 密钥，
  对真实 S3/R2 必 403）；store 索引 name/version 白名单（下载路径目录
  逃逸）+ 流式下载短写检测；mkfs_sfs layout() 死循环（total=33813 实测
  挂死，改二分求不动点）；kunxpkg 注册写失败漏 restore_backup、升级回滚
  不再清空旧版 /res、semver 预发布含 'x' 误判通配、alloc_uid 饱和拒绝
  分配；kunxboot dtb insert_props 容量检查补 insert_off；**构建系统根因
  （cb9ca35 修复不完整）**——linker 行的 rerun-if-changed 仍遮蔽
  include_bytes 跟踪，纯 rootfs 变化不触发内核重编译、陈旧 .rodata 被
  objcopy 重新打包（测试标记「间歇性」丢失），build.rs 显式跟踪
  ramdisk.img。
- 验证：`make` exit 0 / 0 警告；宿主 sync 57 / wireless 105；power 21/21、
  lifecycle-core 38/38、全应用回归 14/14 PASS + 4 项交互抽查。

### 2026-09-08 全面屏手势修复（上滑回桌面/边缘返回失效）

设置应用内底部上滑回桌面、边缘返回完全不生效（返回动画自然也无从播放）。
两层根因，均在 settings 自带输入循环（calculator/stub 模板无滚动问题但缺
鼠标分类）：

- **nav 动作被重绘分支吞掉（主因）**：主循环 `if res.redraw { full_redraw();
  continue; }` 位于导航检查之前，而触摸拖动期间 `ABS_Y → drag_to` 无条件置
  `res.redraw` —— 上滑/边缘返回这批事件必然含移动帧，nav 动作永远被
  `continue` 丢弃（此前偶发生效纯属事件分批的时序运气）。修复：`drag_to`
  返回是否有实际变化，仅变化时置 redraw；主循环把 nav/key/tap 处理移到
  redraw-continue 之前（"系统导航优先"的注释与代码首次一致）。
- **列表滚动吞手势**：底部上滑天然是垂直拖动，被当作列表滚动并在抬起时
  以 `sliding=true` 抑制导航分类。修复：系统手势保留区（底部热区
  `BOTTOM_ZONE`=130px / 左右边缘条 `EDGE_W`=24px）内按下不参与滚动与
  滑条抓取（`pages::pointer_down/drag_to/pointer_up` 的 NAV_EXCLUDE），
  抬起照常分类；保留区内点按不受影响。
- **鼠标/数位板路径缺 classify_swipe**：QEMU GUI 的 virtio-tablet 以
  BTN_LEFT 上报，settings/calculator/stub 的鼠标抬起分支只有 nav_hit 没有手势分类
  （app_kit 同分支有）—— run-gui 下全面屏手势完全不可用。补齐
  TOUCH_PX/PY/US 记录与 classify_swipe 调用。
- 验证：QEMU 实测（UART 触摸注入）——设置页底部上滑 → 退出码=1（Home）
  + `返回过渡：4 frames` 动画播放；边缘返回 → 退出码=0；保留区外起手的
  列表滚动无回归；保留区内点按正常。

### 2026-09-08 打开应用动画增强（ColorOS 连续启动 + squash & stretch）

- **连续启动打断（极光引擎核心场景）**：返回转场（300ms 缩回图标）中途
  点击其它图标不再被吞 —— 以当前中途帧为新背景快照，无缝切换为新应用的
  启动转场（`start_launch` 接受 `Phase::Return`；快照时 `render_frame`
  走 zoom_blit 路径定格返回中的画面）。打断路径调用
  `app_life::return_interrupted()` 清理挂起完成态（BG_RECENTS 残留会在
  下次返回完成时误开最近任务）。转场期间跟手翻页/滑动切页被
  `page_drag_allowed` 与 Swipe 分支屏蔽（转场期桌面不走 draw_page）。
  Resume（后台恢复）转场不开放打断：被恢复应用须保持 SIGSTOP 且
  launcher 侧单槽状态无法同时表达「挂起恢复 + 新启动」。
- **启动白闪 Emphasized 化**：白闪后段 0→255 由线性改为 `Emphasized`
  （慢起-快中-缓收），起闪更缓，减白闪突兀感（路线图 §4 改进项）。
- **启动瓦片 squash & stretch**：放大途中纵向边长 ×(1+0.03·(1−进度))
  瞬时超前、随进度收敛回等比，落位精确覆盖全屏（`launch_blit` 横纵
  分离采样；P2-1 非线性形变的启动瓦片部分）。
- 文档：`docs/animations.md` §3.2/§3.3 数值同步。
- 验证：全量构建通过；QEMU 实测（headless ramfb + QMP screendump + UART
  触摸注入）——① 正式 300ms 参数下启动转场日志 `启动过渡：3 frames`
  （TCG 慢宿主，多帧推进）；② 慢动作构建（临时 1.2s/3s，验证后恢复）
  抓帧序列 10 种 distinct：压暗快照背景 → 白闪爬升 → 应用首帧覆盖，
  无撕裂/残影；③ 三键返回退出 Settings → 返回转场中途点击另一图标 →
  日志命中 `返回转场被新启动打断（连续启动）`，新应用 fork 并完成
  自身启动转场（`启动过渡：4 frames`）。

### 2026-09-07 通宵修复轮（六路并行审查 → 30+ 项修复 + 性能 + 美化）

> 方法：6 个并行审查代理（browser 新模块 / launcher 与动效库 / 媒体类应用 /
> 内核图形动画输入 / settings+OOBE+共享 UI / 性能专项）逐行读码，人工复核后
> 分批修复。全部通过 `make` 全量构建（exit 0）验证。

#### OOBE 严重修复（用户选择静默丢失）

- **`apply_step_defaults` 无条件重置用户选择**：主循环每步「下一步」前都执行
  `apply_step_defaults(step)` 再 `save_step(step)` —— 用户选的英语/取消勾选的
  模块/PIN 锁屏/深色主题全部被重置为默认值再保存（作者注释只豁免了密码步，
  其余步骤全部中招）。现在仅「30 秒空闲自动推进」路径应用默认选择
  （`wait_for_tap_next_button` 返回 4 区分），用户点击推进保留其选择。
- **30 秒空闲绕过校验**：空闲路径直接 `return 1` 不经 `validate_step`，
  密码页留空 30 秒会保存 `fnv1a("")` 空哈希。空闲到达密码步时关闭
  `SEC_ENABLED`，且 `save_password` 在未启用安全时清空 `security_type`
  （防「有 security_type 无 security_hash」→ launcher 开机即锁死）。
- **完成页「进入系统」按钮是死代码**：step 12 从不等待点击直接进桌面，
  `BTN_ENTER` 绘制与命中全部不可达。真实输入路径现在等待点击（30 秒空闲
  自动进入，保留无人值守可推进）。
- **闲置呼吸残影**：Hero 光晕/进度点呼吸逐帧在 BACK_BUF 上混合不恢复基线，
  1-3 秒内光晕饱和成实心绿圈、r=6 外圈永久残留。改为「基线快照 + 每帧
  恢复再绘制」（`HERO_SNAP`/`DOT_SNAP`，页面重渲染时失效重建）。
- **选中脉冲绿 tint 永久残留**：三帧 tint 直接叠加在 canvas 上（96+48+0），
  卡片永久残留 ~50% 绿色。改为每帧先从 BACK_BUF 恢复卡片底图再叠加本帧。
- **`card_selected` 缺 step 11 分支**：外观页点击后局部重绘把新旧卡都画成
  未选中态；step 7 分支未检查 `SEC_ENABLED`（未启用安全时悬停误显选中）。
- **`present_rect` 超界矩形整体丢弃**：光标 overlay 旧+新位置并集 >540px 时
  该帧更新整个丢失（光标瞬移停滞），改为裁剪到屏幕。

#### 应用正确性（用户操作可触发 panic / 数据损坏）

- **files**：存储栏 `render_storage` 写 `[u8;48]` 无边界检查（固定文案 26B +
  三段 fmt_size，1GB 级卷三段 8 字符即越界 panic）—— 扩到 96B + `push_fmt`
  有界追加。
- **sms**：`modem.rs read_state` read 返回值按 usize（失败 -errno 当巨大值
  → `buf[start..n]` panic）；`write_dec(0)` 缺 `i-=1` 写出空串（会话 id=0
  路径缺号 → 所有 id=0 会话共用一个文件互相污染、时间戳变长错位）；
  `now_stamp` 补零定长（显示层按固定偏移取 HHMMSS）；空号码拒绝入库 +
  列表绘制 `title[0]` 空守卫。
- **video / notes**：getdents64 返回值 `n <= 0` 按 usize 比较（负 errno 巨大
  值当成功，继续解析残留旧数据 → 重复条目/越界 panic），按 isize 判负。
- **camera**：`cam_poll_resp` 对 `RESP_BUF` 子切片 `copy_from_slice`（源/目
  重叠 = UB）改 `copy_within`；录像计时 >10 分钟 `b'0'+x` 溢出成非数字。
- **contacts/local**：`write_file` 单次 write 不重试（~130KB FILE_BUF 短写
  即静默丢联系人且文件已截断），改分片写循环。
- **map**：`push_dec` 丢弃负数（传感器姿态/南纬西经渲染为空），经 i128 绝对
  值支持负号。
- **phone**：通话计时分钟钳到 99（>99 分钟 `b'0'+mm/10` 溢出）。

#### browser（远程页面即可触发）

- **DOM 树深无上限**：`<a>` 重复 ~2 万层（64KB 页面即可构造）→ 递归遍历
  （elements_by_tag/text_of/preorder/collect_text）打穿 1MB 用户栈。解析
  阶段 `MAX_TREE_DEPTH=256` 封顶（超限按父级平铺）+ 遍历深度守卫。
- **`<button>` 文本永远为空**：`Node::text()` 对元素节点恒返回 `""`，布局
  与绘制按钮标签为空。新增 `Dom::inner_text()` 聚合子树文本。
- **HTTP 头终止符跨段丢失**：`\r\n\r\n` 被 TCP 分段切开时只在「新增数据」
  内搜索永远匹配不到（正常页面报「响应头不完整」）；改从拼接缝前 3 字节
  扫描 + `\n\n` 终止符按实际长度取 body（原来多吞 2 字节）。
- **图片加载无同源校验**：`same_origin` 是死代码，远程页面 `<img>` 可对任意
  内网/外网地址发 GET（探测/追踪），file:// 页可读任意本地 PPM。预抓取
  过滤 `same_origin || kunxos://`。
- **CSS 长度无上限**：`1e30px` → inf → `as i32` 饱和 i32::MAX → 滚动条/绘制
  坐标加乘回绕簇。`parse_length` 钳制 0..=8192px；max_scroll 钳 1e6 +
  thumb_y i64 中间量；`visible()` i64 比较。
- **`<a><img></a>` 链接失效**：Replaced 原子不计入行内盒矩形，仅含图片的
  链接 0×0 被 retain 丢弃。Replaced 携带 ibox 并 add_ibox。
- **chunked 传输编码**：明确报错（此前把分块元数据当页面内容呈现）。

#### launcher

- **锁屏按压卡死（每帧全屏重绘）**：锁屏时按压命中图标 → `PressIn` 抬起
  路径被 `handle_lock_input` 消费无 `cancel_press` 机会 → 弹簧永不收敛，
  60fps 常驻全屏重绘 + 解锁后图标卡在按压态。`on_press` 加 `!LOCKED` 门控 +
  锁屏消费手势时兜底 `cancel_press()`。
- **启动动画条纹镂空**：`launch_blit` 步进 4px 但只写 2×2 像素（ZOOM_BLOCK
  从 2 改 4 时漏改，zoom_blit 同步改了这里漏了）→ 放大图标每 4px 出 2px
  缝隙。补全整块填充（与 zoom_blit 同规则）。
- **锁屏回拖负坐标回绕**：上滑后往回下拖 `LOCK_LAYER_OFF > 0`，
  `draw_lock_background` 的 `gy as u32` 对负值回绕（顶部渐变变垃圾色）。
  off>0 时跳过 y<off 行（露出桌面语义）。
- **PressOut 吞点击**：回弹未收敛时按下与抬起都被静默吞（滑动结束后立即
  点图标无反应）。`start_press`/`start_launch` 允许从 PressOut 打断。
- **`app_uid` 解析溢出**：超长数字行回绕（debug panic / release 错误 uid），
  改 saturating。
- **控制中心**：跟手下拉速度无钳制（瞬时 10⁶ px/s 冲飞面板进度）钳 ±3000；
  弹簧初速钳 ±4 面板/秒；跟手打开重建玻璃背景（此前沿用上一次缓存的旧
  桌面 backdrop）；滑条拖动每个输入事件写盘 → 松手结算落盘一次。

#### settings

- **拖拽滚动公式两路全错**：`drag_to` 期望绝对 y —— 触摸路径传
  `TOUCH_PY - value`（滚动量 = 手指绝对坐标，下半屏一碰就钳底）；鼠标路径
  传单次 REL_Y 增量（≈ 常量，拖动立钳一端）。触摸改传绝对 y，鼠标改按
  下点 + REL 累计。
- **按压反馈**：设置页所有交互此前无任何按压反馈（对比 OOBE 按钮反馈）。
  新增通用按压高亮（触点软边压暗圆斑写 canvas，抬起从 BACK_BUF 恢复；
  滚动重绘自愈，不污染权威底图）。

#### 内核

- **compositor fb_present 脏区不覆盖内核失效区**：`copy`/`prev_app` 只含
  应用脏区 —— alpha 窗口动画每 2 帧在同一 back 上重复混合（透明度逐帧
  加深）、窗口移走后旧位置残影到应用恰好重绘为止。内核失效区并入拷贝集
  与 prev_app（canvas 为权威底图，先回填再混合）。
- **CNTP IRQ 内串口忙等 ~10ms**：帧统计每 60 帧（0.5s）在中断上下文逐字节
  忙等输出 ~110 字符 ≈9.5ms 且全程 IRQ 关闭，超 8.33ms 帧预算本身，调度
  tick 与 UART RX 全部顺延。改 IRQ 侧置标记、内核主循环线程上下文冲刷
  （`anim::poll_deferred_stats`）。
- **on_finish 回调迭代中重入**：完成回调（链式动画/销毁窗口）在 ANIMS
  迭代循环内执行，回调内 tween/cancel_window 会改写槽位表。改为暂存
  `FINISH_Q`、循环外统一执行。
- **恰好 32 个脏矩形被降级整屏**：ioctl 侧 `>MAX_DIRTY` 才 EINVAL，
  `fb_damage` 却 `>=` 判超限，改一致。
- **perf overlay 重复混合**：overlay 画在 back 且不在拷贝/clean 集，40% 黑
  底每 2 帧累积到底色全黑。绘制前先从 canvas 恢复该区域（新增
  `restore_back_from_canvas`）。
- **ui_framework Image 行距错位**：源宽被钳到目标宽导致每行内容整体错位，
  新增 `blit_strided`（裁剪拷贝宽高、源按原行距推进）。
- **eret/memset/memmove 宽通道**：内核 `memset` 升级 16B `stp`/8B `str`
  （页清零/画布填充路径），`memmove` 无重叠走 memcpy 宽通道、重叠反向
  保序（volatile 免疫 LoopIdiomRecognize 的 `call memset` 自递归变换）。

#### 性能（输出逐位一致的优化）

- **EL0 memcpy/memmove/memset 宽通道**：`shared/ttf_text.rs` 的字节循环
  （所有应用唯一的 memcpy 定义）升级为与内核同款 `ldp/stp` 16B 宽通道 +
  8B 回退 + volatile 字节尾。launcher 每个动画帧仅背景恢复 + flip 就有
  ~4MB 拷贝走此路径。
- **字形缓存扫描局部化**：256 槽线性扫描落在连续 2KB 的键旁路数组
  `GLYPH_KEYS`（此前每字符每帧跨 1KB 步长扫 256KB）；`cache_store` 就地
  写字段（去掉每次未命中 1KB 栈临时）。
- **fill_rect 行填充**：app_kit / launcher / calculator / terminal / stub /
  oobe / settings 共 7 处逐像素 `put_pixel`（每像素 4 次边界比较）改为整体
  裁剪 + 逐行 `slice::fill`（宽通道 memset）。
- **app_kit 字形混合**：三个 TTF 字形路径（cn/bold/icon）共用 `blit_glyph`
  —— 求交一次、直写 BACK_BUF、a==255 直通（此前每像素 4 比较 + put_pixel
  再检查 + 全 alpha 走 blend）。
- **小部件文件读 TTL 缓存**：天气温度/天气文字/电池/充电状态此前每帧
  open/read/close（第二页动画 + 锁屏充电动效 60fps 常驻），加 1s TTL 缓存。
- **滑条拖动不落盘**：控制中心亮度/音量拖动每个 ABS 事件写一次盘（可达
  每秒百次），改松手落盘一次。
- **缓动求解提前收敛**：内核 anim.rs 与 launcher 的 `solve_x` 牛顿收敛即
  返回（每 tween 每帧省 ~20 次 bezier 求值）、二分区间用牛顿结果收窄。
- **杂项**：`clean_needed()` 平台判定缓存（每矩形一次字符串扫描降为一次）；
  frame_tick 复用 tick 时刻（省 1 次 CNTFRQ 读 + 除法）；launcher 输入批读
  48B → 256B（快速滑动 read 次数降 ~5 倍）。

#### 美化

- **强调色统一为 ColorOS 绿**：`0x00B96B`（docs/animations.md 3.5 规范）。
  清除三种绿并存：iOS 绿 `0x22C55E`（ds.rs/app_kit/launcher/browser/
  calendar/contacts/map/weather/stub 等）、iOS 系统绿 `0x34C759`（settings
  开关/图标、clock 开关）、变体绿 `0x1ABB5E`（sms）—— 22 个文件统一。

### 2026-09-07 第二轮（内核测试全绿 + 玻璃带缓存 + 内核级修复）

> 目标：把此前一直红着的内核级无头回归全部拉绿（power 18/21 → 21/21，
> lifecycle-core 0/38 → 38/38），并落地剩余的性能项。`make` 全量构建
> exit 0 / 0 警告。

#### 内核修复

- **双 shell 劈开 UART 输入流**：bootargs `shell` 先拉起一个内置 shell，
  /init 退出后又回退拉起第二个 —— 两个 shell 并发读同一 UART，自动化注入
  的命令按字节被劈成两半（`cat /proc/pressur|e`），双方都不执行。init
  退出路径在 BOOT_SHELL 已有时不再重复 spawn。
- **lifd 多段日志被 log() 强制换行拆行**：`[lifd] 已登记 ` 与
  `appA pid=N FG` 被拆成两行（第二行丢失 [lifd] 前缀），影响日志可读性
  与自动化匹配。新增 `log_raw`（无换行），10 处拼接式消息（已登记/
  已转入后台/已回前台/已终止/已挂起/已到期/通知已投递/已延迟/已丢弃/
  后台任务已授予）改整行输出；补上立即投递路径缺失的 `通知已投递` 日志。
- **风暴饿死 UART RX**：转储 ~150 字符在 IRQ 关闭下逐字节忙等 ≈13ms，
  PL011 16 字节硬件 FIFO（115200 下 ~1.4ms 填满）溢出即丢输入字节。抽出
  `uart_rx_drain()`：风暴转储间隙主动排空，且风暴 episode 每 256 次同号
  重入排空一次（一次 volatile 读，常态零开销）—— 注入帧不再按帧丢失。
- **启动期电源自检结论被安静模式吞掉**：违反 kinfo! 自身的日志规范
  （「自检 PASS-FAIL 必须可见」），`[kernel] 电源自检完成：N 个失败` 改为
  无条件输出；shell `power selftest` 输出与启动期格式统一
  （`自检完成: N 个失败项` → `自检完成：N 个失败`）。

#### 服务

- **pushd 恢复开机自启**：本地通知队列（/data/push/in → lifd DELIVER）
  随 pushd 被移出 init 启动列表而整体不可用，通知推送链路（含
  lifecycle-core 推送断言）重新接通。

#### 测试修复（测试与实现脱节）

- **power_test**：`dvfs switch logged` 依赖 kinfo 门控的 `[power] dvfs`
  行 —— 安静模式默认开启后永远无法命中；改为 shell `verbose on`（热节流
  段前，等待启动日志洪水沉降）+ bootargs 补 `selftest`（启动期自检根本
  未运行）；(shell) 自检字节串与实现漂移按统一后格式匹配。21/21。
- **lifecycle_core_test**：启动语中文化后断言仍匹配英文（lifd/pushd
  starting、registered/background/foreground/granted/denied/expired/
  suspend/terminated/deferred 等十余处）；`BTASK 3` 前缺 `BTASK_END`
  （每应用上限 2 已满，短任务恒被拒，到期路径不可达）；「notify
  forwarded」期望的日志此前不存在。38/38。

#### 性能（第二轮）

- **状态栏 / Dock 玻璃带缓存**：玻璃源只是壁纸，键 =（调色板，主题），
  命中时按行 memcpy（~4.2 万重像素/帧的降采样+模糊+着色归零）；换壁纸
  交叉淡化期间键逐帧变化、自动回落实算。
- **最近任务卡片启动快照残影**：卡片点击启动不再播 150ms 关闭动画
  （快照含面板残影），直接复位面板状态后从卡片位置放大。

#### 回归稳定性（风暴窗口丢帧 → 注入验证重试）

- 全应用回归 page-1 应用（map/store/video/notes）稳定 FAIL：注入的滑动
  MOVE 帧在内核风暴窗口丢字节 → 手势判定退化为原地长按（Gesture::None
  静默）→ 翻页未生效，脚本却盲目递增本地页码状态，随后的瓦片点击命中
  第 0 页的其它应用。修复两层：launcher 翻页结算打印 `[launcher] page=N`
  （此前翻页对外不可观测）；回归脚本 goto_page 改为按页码日志确认、
  未生效重试（最多 4 次），不再假设滑动必然生效。14/14 PASS 恢复。

- **丢抬起事件看门狗**：跟手拖拽态（翻页/锁屏/最近任务/控制中心/编辑
  拖拽）激活但按压态持续 2 秒无任何输入事件 → BTN_TOUCH up 已丢失（注入
  帧被风暴窗口吞掉/设备异常）—— 强制清除按压态并走既有「抬手兜底」收敛
  全部跟手状态。此前家族性卡死（120Hz 预算重绘不退）无任何自愈路径；
  正常长按（无拖拽态）不受影响。

#### 其它

- 删除死代码模块 `kunxcore/src/input_stack.rs`（无调用点，且 grab 裸
  窗口 id 有复用隐患）。
- launcher：锁屏快扫解锁阈值与跟手结算对齐（>18% 屏高或 >600px/s，此前
  任何 ≥30px 上滑轻扫直接解锁）。

### 2026-09-07 第四轮（最近任务/控制中心热点 + 轮询降频）

- **最近任务压暗层烘焙**：面板打开期间桌面静止（仅卡片运动），打开后
  首帧把「桌面 + 全屏压暗」存入缓存，动画帧整块 memcpy 替代 52 万像素的
  逐像素 blend；壁纸交叉淡化期间自动回落实算。
- **控制中心圆角混合盒预检**：cc_fill_round_blend 补四角盒外快速路径
  （与 glass_rect_style 同款），滑条/开关 ~80% 像素免每像素浮点求值。
- **后台事件轮询降频**：lifd launcher_evt 每 4 帧轮询一次（此前每帧一次
  open(ENOENT) 空 syscall）。

### 2026-09-07 延长轮（四路并行审查 → 40+ 修复 + 构建系统根因）

> 方法：4 个并行审查代理（kunxserv 全部服务 / 小应用群 / 内核 shell+pkg+ota+
> anim+compositor / boot+HAL+pkg+tools）逐行读码。每批修复后构建验证。

#### 构建系统（本回合多次「测试 FAIL」的真根因）

- **`build.rs` 的 `rerun-if-changed=ramdisk.img` 遮蔽 include_bytes! 依赖
  跟踪**：声明该路径后，ramdisk 内容变化时 cargo 实测会跳过重编译，objcopy
  把陈旧 .rodata 重新包装成 kernel.img —— test-sfsrace 的标记间歇性丢失
  （三复现）与「同镜像旧内核 PASS / 新内核 FAIL」的二分假象均源于此。移除
  该行（标准 include_bytes 依赖跟踪足够）；test-sfsrace/test-modem/test-audio
  配方改为显式 mkfs 重建镜像（双保险）。修复后 make test-sfsrace 连续 3/3 PASS。

#### 内核

- **ota `write_file_from` 在 close 之后 fsync**（fd 已释放 → fsync 恒 -1）：
  OTA 安装写入完全成功也报失败，功能整体不可用。fsync 移到 close 前。
- **ota `load_otp` 缓冲 2KB < 8 扇区(4KB)**：blk 层长度检查拒绝 → load_otp
  永远失败，反降级楼层静默丢失。缓冲按磁盘几何（4KB）分配。
- **ota `wipe_dir` 删除-前移**：unlink 后 idx 仍递增 → 隔一个删一个
  （清除数据残留用户文件；/data/oobe/.done 落偶数位时 OOBE 不重置）。
  改同位重读 + 失败护栏；顺带替换该函数的 from_utf8_unchecked。
- **shell `cmd_power`**：所有成功子命令都附带打印「自检完成：N 处失败」，
  且与 power.rs 的 UART 行（「N 个失败」）双份输出——改为仅 selftest 打印。
- **anim `report_frame_stats` n==0 下溢**（STATS_PENDING 跨上下文消费窗口）：
  dev 构建溢出 panic / release 打垃圾统计。加 n==0 早退。
- **anim 收尾帧**：最后一个动画到期后不再合成呈现 → 终值只标脏区，屏幕
  停在中间值（如淡出残影）直到下次输入。收尾帧立即 composite+present。
- **shell `source` 作为管道段拒绝**：cmd_source 逐行再入 run_pipeline，
  复用同一对静态 INPUT/OUTPUT_BUF，外层管道输出被嵌套改写。
- **perf overlay**：恢复集取当前 ∪ 上一帧矩形（文本长度逐帧变化）；overlay
  矩形并入 present clean 集（真机缓存一致性）。

#### 服务

- **rild `after_colon` 无冒号行必崩**（`&line[len+1..]` start>end）：真实
  GSM 模块默认每 2s 上报无参数 `RING` → rild panic 退出，通话/短信全失效。
- **rild `write_dec`**：v==0 写出空串（AT+CMGR 缺索引）+ 缓冲尾 8 字节对
  9-10 位 idx 长度不匹配 panic。补 i-=1 + 按缓冲截断。
- **lifd**：4 处漏网 `log()` 多段消息（应用已退出/通知已投递/压力挂起/LMK
  击杀）改 log_raw；`find_by_pid(0)` 命中空槽哨兵（对 pid 0 发 SIGSTOP =
  进程组语义殃及全部守护）加守卫；畸形 reg 文件提前 return 不 unlink →
  每 50ms 永久重扫，补删除。
- **alarmd 铃声包络基准错位**：段内位置误用段起始块号 —— 段 A 每块全深度
  颤音、段 B 幅度 0.4% 不可闻。改（块序号-段起始）×块帧数+块内偏移。
- **sensord `RESET` 空操作**：命令只打印，计数继续累加 —— handle_command
  透传检测器可变引用，真正重置（kunxhal RaiseDetector 补 reset）。
- **positiond 模拟路线把度当弧度传 cos**：经度漂移方向反转、速率偏差
  ~30%。补角度转换。
- **logd 环形截断永不发生**：file_size 负返回按 usize 加法回绕。按 isize
  判负 + saturating。
- **camerad `fmt_dec` 下溢护栏**（定宽缓冲位数超限时 i 回绕）。
- **pushd 通知丢失窗口**：lifd 未就绪时 /data/life 不存在 → 写命令信箱
  失败且请求文件已删。补建目录重试 + 仍失败写回请求文件。

#### 应用

- **calendar `save_new_event` 用 4KB 栈缓冲开 Store**：events.blob >4KB
  （约 10 个事件）后保存恒失败（误报存储已满）；遇墓碑 compact 时以截断
  内存半覆盖 blob 文件 → idx/blob 永久不一致。复用静态 256KB blob。
  另修：新建/同步页第三个输入框点不中（命中区扩到 y<350/290）；日视图
  标题按 NUL 截断绘制（160B 填充区产出 notdef 字形）。
- **cloud `ensure_credentials` seed 无界追加**：账号+设备 ID 超长即索引
  越界 panic。补上界断开。
- **cloud 远程锁定被本地点按解锁绕过**：现要求长按 2s 确认 + 落审计记录
  /data/cloud/unlock.log（完整鉴权需服务端 unlock 流程，产品侧待定）。
- **recovery 菜单 reboot 用错系统调用号与命令**（169=gettimeofday、
  0x4321FEDC=POWER_OFF）→ 永不重启。改 142 + 0x1234567。
- **probe**：剪贴板/TCP recvfrom 的 `usize > 0` 判正（负 errno 巨大值 →
  切片 panic）+ 一组恒真/恒假死检查（wait4/openat 失败检测失效）按 isize。
- **clock 闹钟行开关只判 y 不判 x**：行上半部任意 x 都翻转开关并落盘。
- **store 下载重试只 lseek 不截断**：上次尝试的残留尾部让整包验签必败。
  重试前 ftruncate。

#### HAL / boot / pkg / tools

- **kunxhal plat_qemu `render_yuv420` 时间戳条位移下溢**（15 - idx/4 在
  idx≥64 时 usize 下溢）：overflow-checks 构建每帧 panic，release 花屏。
  改 31 - min(idx/4,31) + saturating 基址；同文件 ap_cfg 的 ssid_len/psk_len
  在截断拷贝**之前**赋值（消费者按 len 切片越界），改按上限截断。
- **kunxhal plat_pi4 mailbox 响应解析无边界**：畸形 val_size 令 p 越过
  64 项缓冲 → display_init 启动路径 panic。补边界与单 tag 上限。
- **kunxhal motion RaiseDetector 冷却恒假**（以本次 Raising 起点为基准，
  1.5s 超时 < 2.0s 冷却）→ 抬腕一生只触发一次。新增 last_event_t 基准。
- **kunxboot DTB 容量不一致**：MAX_DTB_FILE=576KB vs fixup scratch 128KB
  —— 128KB~576KB 的合法 DTB 启动卡死。收紧上限 + 编译期断言；`#cells`
  属性 len<4 越界读补检查。
- **kunxpkg `append_path` 用逗号、`rollback_files` 按 NUL 切分**：整个
  files 串被当成一个路径 —— 卸载/回滚从不删除已解压文件。统一 NUL。
- **kunxpkg remove/perms/rollback 的 CLI 包名无白名单**：`pkg remove ../..`
  经 /res/../.. 清扫根目录文件。三个入口补 valid_name。
- **kunxpkg perms 授权/撤销前缀匹配**：STORAGE 前缀误命中 STORAGE2 行并
  改写。改完整字段（perm+':'）匹配。
- **kunxpkg fsio `list_dir` 单次 getdents64**：>170 项目录静默漏项，改
  循环读至 EOF。
- **tools/mkfs_sfs.py `layout()` 收敛振荡**（特定 total 死循环）改单调
  推进；MAX_BLOCKS 按写入端实际能力（直接+一级间接）封顶（此前按二三级
  间接数学容量放行，>4.29MB 文件 pack_into 越界）。验证：输出与提交镜像
  逐字节一致。
- **tools/mkdisk.py** 分区容量文案与实际几何对齐。

### 动画渲染提速（2026-09-07 轮）

动画帧的三大逐像素热点改为缓存/整块拷贝，输出逐位一致：

- **launcher 背景缓存**：文件夹/翻页/控制中心/最近任务/按压等桌面动画每帧
  都经 `draw_desktop_full` 重画整屏壁纸背景（54 万像素渐变+波形逐像素数学，
  TCG 下 50~150ms/帧）。现在按调色板键缓存渲染结果（`BG_BUF`），键不变时
  （绝大多数动画帧）整块 memcpy；换壁纸交叉淡化期间每帧键变化、按原路径
  重渲染并更新缓存。`draw_background_rect`（时钟小组件每秒局部刷新）同样
  命中缓存时按行 memcpy。
- **启动转场快照压暗一次化**：`launch_blit` 每帧对全屏快照做 LUT 压暗扫描，
  而快照在转场期间不变 —— 现在 `start_launch` 快照后就地压暗一次
  （`darken_frame_buf`），转场各帧背景降为纯 memcpy。就地修改安全：
  `FRAME_BUF` 的其它读者（Return/Resume 转场缩放）使用前都会重新快照。
- **转场缩放采样降维**：`zoom_blit`（启动/返回/恢复转场）的列采样 sx 此前
  每行重算（240 行 × 135 列 i64 乘移位/帧），改为每帧预计算一次列采样表；
  块填充的末行/末列边界检查外提到块级，内层循环无分支。
- **内核合成器 alpha 混合去除法**：`blend_pixel` 每像素 3 次整数除法改为
  `(v + 1 + (v >> 8)) >> 8` 位运算（0..=65025 全域穷举验证与地板除逐位
  等价，输出不变）—— OOBE 页面淡入淡出、Home Indicator 等覆盖层动画的
  合成热路径直接受益。缩放窗口合成的源 X 采样表在构建时钳制到源宽内，
  行内逐像素不再做边界分支。

### 应用打开提速（2026-09-07 轮）

点击图标到应用首帧的实测耗时（QEMU TCG 慢宿主，Settings）：**13.1s → 2.8s**。
三处根因与修复：

- **settings 整屏提交逐像素处理（占 ~4s）**：默认亮度 85 使 `flip()` 每帧走
  逐像素 `lut_px` 路径，且每像素调用 `invert_on()`/`contrast_on()` 配置查询。
  现把亮度×反色×对比度合成为一张 256 项通道 LUT（参数不变时复用，参数为
  默认直通时仍走 memcpy 快路），整屏提交降为查表；输出逐位一致。
- **fontdue 全量字体解析（占 ~3s）**：`cn.ttf`/`bold.ttf` 子集此前按整文件
  文本收集字符集，被**未接线的拼音 IME 字典**（`shared/pinyin_dict.rs` 的
  GB2312 一级全字表）撑到 3538 字/1.6MB。`make_fonts.py` 现显式排除该死代码
  文件（接入 IME 时需移除排除并重建），并把 `tools/kunxrepo` 的包描述生成器
  纳入扫描（store 显示的 Packages 文案）、显式解码 `\xNN` 字节转义文案；
  子集降至 1731 字/742KB，解析时间减半。缺字审计确认运行期文案全覆盖
  （源字体本身不含的阿语/文本 emoji 与旧版一致，emoji 走 Phosphor 图标路径）。
- **启动期冗余首帧（占 ~4s）**：settings 此前「先画一帧像素字 → 加载字体 →
  再画一帧」，首帧立即被覆盖且整屏提交最贵。现只画一次（字体就绪后），
  字体加载期间屏幕由 launcher 的启动占位页覆盖。
- 配套：`ttf_text::read_file` 读取分块 4KB → 32KB（1.6MB 字体 400 次
  syscall 降至 ~50 次）；`font_init` 增加一行 `[perf] font_init` 诊断；
  补上 oobe `finish.rs` 引用但未定义的 `draw_glass_dark`（工作区预存编译
  破损，全量重编时暴露）。

### 全系统浅色主题：应用白色背景打底（2026-09-07 轮）

- **规范**：页面基底纯白（渐变 0xFFFFFF→0xE9EDF3）、主文字 0x1A1C22、
  次要文字 0x6B7280、卡片描边 0xD9DEE6、分割线 0xE5E9F0；强调绿/
  危险红/警示琥珀保留，彩色表面（红/绿/琥珀玻璃）上的文字保持白色。
- **shared**：`app_kit` 的 `glass_dark()` 固定返回 false（不再读
  /data/oobe/theme），默认调色板浅色化，玻璃按钮 accent 玻璃白字/
  浅玻璃深字；`glass.rs` 的 `STYLE_LIGHT` 描边改浅灰以在白底保持轮廓；
  `nav.rs` 底部导航栏改浅色玻璃 + 深色图标；`ds.rs` 固定浅色模式。
- **apps**：全部 GUI 应用（launcher、oobe、settings、files、cloud、
  contacts、clock、music、mail、sms、browser、weather、calendar、
  gallery、map、notes、video、camera、phone、lcapp、calculator、
  store、stub、terminal）背景改白色打底、白字改深色、深色玻璃改浅色
  磨砂。特殊：terminal 白底深字 + ANSI 过亮色压暗；video/camera 取景/
  播放画面保持黑色（内容非主题）；map 地图要素整体浅色化。
- **主题开关**：launcher/oobe 的深浅色选择功能保留但系统渲染固定浅色
  （THEME_DARK=false，读取配置时忽略深色字段）。无头测试/诊断应用
  （probe、modemtest、audiotest、audiolat、audiomix、crashme、sfsrace、
  recovery、sleepy）无界面不涉及。boot splash 为 bootloader 黑底
  Logo，未改动。

### 挂起缺陷清剿 + 应用内动效（2026-09-07 轮）

- **内核 heap**：slab 半满状态双重释放防护补全（空闲链表遍历查重，此前
  仅页标签拦截「页已归还/改作它用」的陈旧指针）。
- **内核 signal**：pthread 工作线程信号帧边界放宽 —— 工作线程栈来自用户态
  mmap、不在主栈 ASLR 区间，此前收信号一律被误判「栈越界」按默认动作
  误杀；现接受用户空间内已映射可写的帧（LAZY 落地/COW break 仍强制）。
- **内核 fat32**：目录簇链不可增长 → `find_free_slot` 走到链尾时追加
  新簇扩容目录（分配即清零，根目录同样适用；卷满保持原错误）。
- **内核 interrupt（严重）**：风暴护栏对 PPI 写 `GICD_ICENABLER` 在 QEMU
  下会把调度器时钟中断真正关掉（架构规定该写对 SGI/PPI 无效，QEMU 照单
  全收）→ 护栏触发即整机冻结。现仅对 SPI（irq≥32）执行屏蔽写；PPI 风暴
  episode 实测自行消散，只静音诊断。
- **launcher**：fb0 ioctl 分辨率合法性钳制（异常值致 `W*H*4` 回绕为 0、
  mmap 恒 EINVAL 的可疑路径）；启动布局转储 `[launcher] tile/dock`。
- **动画**：calculator 按压下沉/松开闪光/结果弹出；clock 标签指示条滑动
  （均按 docs/animations.md 3.1 节奏，全整数运算）。
- **测试**：apps_regression.py 按布局转储重写（真实坐标、模块过滤记
  SKIP、返回语义修正）。

### 流畅度：鼠标跟手 + 交互调度专项（2026-09-05 轮）

对标鸿蒙（交互线程调度提速 / 唤醒抢占）与 ColorOS 15（潮汐引擎触控加速、
极限稳帧）修复「鼠标不跟手、应用卡顿」：

- **根因一（不跟手）**：约 14 个 kit 应用光标移动只画进画布、从不提交呈现，
  光标要等下次全量重绘才上屏。`shared/app_kit.rs` 的 `cursor_overlay_move()/
  cursor_overlay_redraw()` 现在随写随以小脏矩形（旧+新位置 ~26x26）提交
  FBIO_PRESENT，输入→画面单次小矩形呈现即时上屏。
- **根因二（卡顿）**：settings/calculator/store 等应用每次移动提交**整屏**
  脏矩形，内核在屏蔽中断的 ioctl 内做整屏 canvas→back + 整屏 ramfb 拷贝
  （每次移动 ~4MB）。全部改为小矩形提交；stub 补齐缺失的 FBIO_PRESENT
  管线（此前 overlay 光标根本不上屏）。
- **内核 `compositor::fb_present`**：prev_app（2 帧并集）语义保持不变——
  `present()` 交换 front/back 后下一次写入的 back 是 2 帧前的旧缓冲，并集
  负责补齐两帧间变化；ramfb 呈现指向整个 back，任何"只补小矩形"的捷径都
  会把陈旧区域整屏翻上扫描输出（全屏闪烁）。应用侧小矩形提交配合该并集
  正确工作：光标移动通常只拷贝 ~676×2 像素，仅全量重绘后的第一次移动
  退化为整屏拷贝（~1ms，可接受）。
- **内核调度（`sched_cfs`）**：新增唤醒抢占（`wakeup_preempt`，Linux
  `check_preempt_wakeup` 思路，粒度 2ms）——交互/实时任务唤醒后无需等
  后台任务用完 10ms 时间片；`yield_now`/`block_current`/`stop_current`
  作为调度决策点清除累积请求，避免多抢占一次。
- **输入触控加速（`input.rs` + `sched_cfs::boost_tid`）**：指针/按键事件
  到达时把最近读取输入的前台 UI 任务 vruntime 拉到全局最小（CFS
  `place_entity` 思路）并发起唤醒抢占，限频 2ms——交互任务即时抢占后台
  负载。
- **输入低延迟**：修复 poller 竞态（登记后、注册睡眠前到达的事件此前会把
  帧循环白睡一个 16ms 间隔，poller 改为不消费登记）；指针移动事件积压过半
  时合并丢弃最旧完整移动帧（Android 输入批处理思路），应用繁忙时光标不再
  「追赶式」爬行。
- **桌面每秒整屏重绘根因（launcher）**：首页时钟小组件含秒针，此前每秒置
  `NEEDS_REDRAW` → 整屏 `render_frame()`+`flip()`（壁纸+毛玻璃+图标+TTF
  全量渲染，TCG 下 50~150ms），期间主循环被占死、鼠标每秒顿一下 —— 这是
  「桌面鼠标非常卡」的主因。新增 `redraw_clock_widget_fast()`：只重绘小组件
  矩形（矩形内壁纸背景 `draw_background_rect` + 玻璃卡片 + 表盘 + 脏矩形
  flip 呈现，~5ms）；分钟 tick 在首页同样改走状态栏 + 小组件局部路径。
  光标落在小组件矩形内时随帧补画 overlay（矩形外不补，防 save_under 把
  光标像素存成背景留下拖影）。
- 验证：`make kernel` + `make launcher` 0 错误；QEMU 截图实测——桌面静止时
  相邻秒帧差异仅剩表盘小区域（此前为整屏），1600px/s 匀速注入下光标往返
  精确到折返点、无累积滞后；闪烁/黑框（prev_app 回归）已修复并回归通过。

### 动效：共享运动库 + 壁纸淡化 / 锁屏充电动效 / 滑条数值气泡 / 设置页转场（2026-09-05 轮）

对标 ColorOS 15（幻光影流动）与鸿蒙页面转场规范补齐 P0 动效（路线图见
`docs/animations-roadmap.md`，数值并入 `docs/animations.md` §3.11~3.14）：

- **共享运动库 `shared/motion.rs`**：贝塞尔缓动（8 曲线，牛顿+二分，与内核
  anim.rs 同算法）+ 阻尼弹簧（半隐式欧拉，含 `retarget` 打断接续——当前值/
  速度无缝交给新目标，springMotion 接续的应用侧等价物）+ 进度/三角波呼吸
  辅助。launcher 与 settings 经 `#[path]` 引入（同 nav.rs 模式），应用侧
  动画从此有了零分配公共底座。
- **壁纸切换交叉淡化**（launcher）：应用返回前台时重读桌面偏好，壁纸/主题
  调色板变化即播 300ms `EaseOut` 行级调色板混合（逐像素零额外开销）——
  设置应用改外观回桌面即时生效，不再需要重启 launcher。
- **锁屏充电动效**（ColorOS 幻光影流动风）：呼吸光晕电池药丸（三角波 1.2s，
  三环光晕）+ 双圈错峰涟漪（周期 1.6s、圈间滞后 0.8s，半径 18→64 淡出）+
  电量百分比；充电状态 1s 缓存轮询，涟漪圆环为整数中点圆 alpha 混合（无
  sin/cos）。
- **控制中心滑条数值气泡**：拖动亮度/音量滑条时上方弹出玻璃药丸实时显示
  百分比，松手保持 900ms。
- **设置应用页面切换转场**（鸿蒙页面转场规范）：推入/弹出 240ms
  `FastOutSlowIn` 左右位移——新页滑入、离场页以 2MB `SNAP_BUF` 整帧快照补底，
  行级平移 + 10px 接缝阴影（无第二渲染缓冲，转场期 8ms 帧预算同步推进）。
- 验证：`make launcher` 全应用 0 错误；内核路径未动。

### 修复：第二轮独立复查修复轮（2026-09-05）

五路并行代码审查（内核核心/文件系统存储/网络 IPC/GUI 应用/服务 init pkg）→
33 项修复，关键项（完整清单见 `BUG_REPORT.md` 第十四节）：

- **Critical**：`alloc_page` 成功路径早退跳过 `irq_restore`（未提交改动引入），
  单次页分配即把中断屏蔽态泄漏到整个 syscall 执行期。
- **eret SPSR 白名单清洗**：堵住「伪造 rt_sigreturn 帧携带 DAIF 掩码 /
  EL0 直写 DAIF → 单核整机冻结」的无条件 DoS（汇编路径 + sigreturn 双层）。
- **socketpair/msgq 对端关闭语义**：EOF/短写/关闭复检（此前永久挂起 +
  槽位复用后跨连接数据串扰）；IPC 引用计数全部加 IRQ 屏蔽窗口。
- **文件系统**：fat32 目录簇链成环冻结、目录项块号伪造校验（防位图写入）、
  数据位图日志暂存失败回滚（防掉电后跨文件踩踏）、fs_lock 单 CAS 取锁。
- **TCP**：close 不再覆盖未确认数据段的重传登记（防连接静默失步丢数据）。
- **用户态**：launcher 死 pid 复活假死、calculator/stub poll_key 越界、
  audiod 死客户端回收、readv/writev 预校验内存放大、rild 短信存储改追加写、
  kunxpkg/kunxverify 多证书链解析、mkfs_sfs 超级块版本号遮蔽等。
- 验证：`make` 0 警告；宿主测试 sync_tests 57 / wireless 105 全过；QEMU
  `-kernel` / `run-disk` 冒烟 0 PANIC。

### 修复：ctl 命令信箱启动期 open 失败刷屏（2026-09-05）

- `alarmd` / `camerad` 以 `O_RDWR`（不带 `O_CREAT`）轮询各自的
  `/var/run/*.ctl` 信箱，而信箱要等客户端（clock / camera 应用）首次写入
  才存在 → 启动后主循环持续 open 失败，`[vfs] open 失败` 行与其他进程
  串口输出交错（还会提前耗尽 vfs 限流的前 24 行配额，掩盖真实错误）。
- 修复：两服务启动即创建信箱文件（`O_TRUNC` 顺带清陈旧命令）；alarmd
  轮询从「读后 unlink」（文件消失后失败复现）改为读后 `ftruncate(fd, 0)`
  清零，信箱常驻（camerad 原本就是 ftruncate 语义）。顺手清除 alarmd/
  camerad panic 路径的双分号与 `shared/tz.rs` 未使用变量告警（0 警告）。
- 验证：`make` 全量构建 0 警告；QEMU 无头冒烟启动日志中
  `alarmd.ctl`/`camerad.ctl` open 失败完全消失，仅剩合法的一次性首启探测。

### 修复：独立复查修复轮（2026-09-05）

四路并行代码审查（内核核心/文件系统/GUI 应用栈/服务与应用）+ QEMU 实机复现，
与第十二轮互补，共修复 29 项，关键项（完整清单见 `BUG_REPORT.md` 第十三节）：

- **桌面起不来根因**：陈旧构建产物中 launcher（396KB）超过旧 execve 读入
  上限 256KB → execve 静默失败、init 随即退出。`make clean` 失效（clean 目标
  引用已删除的 displayd 变量，永远 exit 2）导致陈旧产物残留。两者均已修复，
  execve 失败路径保留诊断输出。
- **内核安全**：socketpair/clipboard_get/statx/rt_sigaction-old 写用户内存
  改可写校验（RO/COW 页 EL1 裸写 = 单 syscall 杀内核）；rt_sigreturn 清洗
  用户可控 SPSR（伪造 EL1h 可 eret 进 EL1 挂死内核）；OTA 变更命令限 root；
  wait4 ABI 修正（options 在 x2，真实 bionic 的 WNOHANG 此前失效）。
- **内核正确性**：SFS fs_lock 改 (CPU,TID) 双令牌 + 死亡持有者抢锁自愈
  （原 CPU 令牌被任务迁移破坏，SMP 并发入临界区）；被杀任务清理 SLEEPERS
  （假唤醒 + 32 槽泄漏）；reap 存活护栏提前（SMP 先释放地址空间 UAF）；
  dup3 越界目标泄漏引用；文件 mmap 游标与匿名游标重叠互踩 PTE；mmap_anon
  失败回滚游标；virtio-blk reset_ring 完整设备复位（旧请求迟到完成被误判
  新请求成功）；SFS 事务 copy-out 失败阻塞日志 head 前移（丢元数据）；
  truncate 写 inode 失败中止事务；pkg uninstall 路径穿越（删任意文件）；
  加密卷冲刷按设备路由（原全局单父冲错盘）。
- **GUI**：oobe present_back_rect BACK_BUF 越界 panic（点「选择应用」页
  任意一行必崩）；oobe 30 秒空闲判定把移动鼠标也算无输入（强推下一步）；
  launcher flip() 脏矩形对 BACK_BUF 尺寸钳制（静默越界读）。
- **服务/应用**：syscall 返回值按 usize 判负族第二批量清剿（wifid/rild/
  settings/store/probe/phone/contacts/lifd/logd/audiod 共 11 处，读错误
  即 panic 或吞错误数据）；networkd/wifid 改常驻（原 0.6s/5s 自退后所有
  Wi-Fi/网络命令静默丢失）；wifid CONNECT/SAVE 长度校验 + 配置文件名净化；
  rild 级联短信按 ref_id 匹配 + 槽位 TTL 回收；settings cloud_login 口令
  缓冲扩容；files fmt_size 大尺寸显示截断；init 回收守护进程僵尸。
- 验证：`make` 全量构建 exit 0 / 0 警告；宿主测试 jpeg 7 / wireless 105
  全过；QEMU 无头与 run-disk GUI 双模式冒烟 0 PANIC；launcher_test 全流程
  （OOBE 13 步 → 桌面 → 点 Clock → 启动/返回转场 → 二次点击存活）通过。

### 新功能：Phosphor 图标字体接入（软件图标 + Emoji）（2026-09-05）

- **码点表 `shared/icons.rs`（source of truth）**：应用桌面图标 / 天气图标 /
  通用 UI 图标 / Emoji→图标映射共 228 个 Phosphor 码点，取自
  @phosphor-icons/core 2.1.1 官方 meta（与 fonts/Phosphor-Fill.ttf /
  Phosphor-Bold.ttf 的 Version 2.1 一致，两字重同码点）。
- **软件图标**：`tools/make_icons.py` 改为用 fontTools 读取 Phosphor-Fill
  矢量轮廓（quadratic/cubic 展平 + even-odd 填充挖洞），按 icons.rs 的
  应用码点渲染 8 倍超采样贴图，替换手绘几何字形；桌面/Dock 21 个应用
  图标即真实 Phosphor 设计（`shared/app_icons.rgba` 已重生成）。
- **行内 Emoji**：`font_render` 新增两个图标字体槽（/fonts/icon-fill.ttf
  实心、/fonts/icon-line.ttf 粗线）。文本路径上 emoji 码点经
  `icons::emoji_icon` 自动映射为 Phosphor 单色图标（359 个 emoji 码点 →
  210 图标），VS16/ZWJ/肤色修饰/区域指示符渲染为零宽空位；未映射 emoji
  与旧行为一致（退回常规字体）。内核 `ui_framework` 与全部应用的
  `font_init` 均加载图标字体。
- **构建**：`tools/make_fonts.py` 新增 `build_icons()`（从 icons.rs 提取
  PUA 码点生成 icon-fill/icon-line 子集，57KB/69KB）；Makefile 接入
  rootfs `/fonts/icon-fill.ttf`、`/fonts/icon-line.ttf` 并把 icons.rs
  加入 SHARED_SRC。
- **修复**：字形缓存键字体选择器与 px 位段重叠（常规/标题/图标字形在
  缓存中互撞）——选择器移至码点上方独立位段。
- 验证：宿主栅格化测试全过；QEMU 实机截图确认桌面 Phosphor 应用图标、
  浏览器「关于」页 Emoji 行内渲染（☀️⛅❄⚡❤⭐🔔📅📁💬👍😀 等映射为
  单色图标）。

### 修复：全量审查修复轮（2026-09-05）

6 路并行代码审查（内核内存/存储/网络 UI、shared 库、全部应用与服务）后修复
30 项正确性 bug，关键项：

- 内核：恢复 `reap` 中丢失的 `reap_pid` 调用（任务槽位/内核栈泄漏回归）；
  SFS `MAX_DEVS` 2→4（第三卷挂载复用槽位 0 摧毁根 FS）；FAT32 每卷独立根
  表项；`wait` 丢唤醒竞态；`spawn`/`insert` 调度锁 IRQ 死锁；`execve` 终止
  兄弟线程；gdbstub/virtio_snd 外部输入 panic；browser CSS 远程 panic 等。
- shared/应用：字体大字形截断 panic、read/lseek 失败值按 usize 放行族
  （lifecycle/audio/sms/modemtest/ttf_text）、cloud/kunxpkg/settings/mail
  缓冲越界、launcher/oobe 防御守卫。
- 完整清单见 `BUG_REPORT.md` 第十二节。验证：`make` 全量构建 0 错误 +
  宿主测试（jpeg 7 / wireless 105 / sync_tests 57）+ QEMU `-kernel` 与
  `run-disk` 双模式冒烟 0 PANIC。

### 新功能：桌面应用真图标贴图（2026-09-05）

- **此前**：launcher 桌面/Dock 的应用图标是"圆角矩形底色 + 白色几何线条"
  的程序化占位绘制，系统没有真实图标资源（唯一位图资源是鼠标指针）。
- **新增 `tools/make_icons.py`**：按 APPS 表顺序为 21 个应用逐个绘制
  56×56 图标（8 倍超采样绘制 + LANCZOS 缩放，白色字形 alpha 蒙版 +
  纵向渐变圆角底），写出 `shared/app_icons.rgba`（裸 RGBA8888，263KB）
  与预览拼图 `tubiao/icons_preview.png`。生成物随源码提交，构建不需要
  Python（Pillow 仅在重跑生成时需要）。
- **新增 `shared/app_icons_sprite.rs`**：图标数据访问模块，顺序与 APPS
  表严格同序。贴图随 rootfs 以 `/share/app_icons.rgba` 提供，launcher
  启动时读入（`install()` 零拷贝借用）。**不放 ELF 内嵌**：263KB 会把
  launcher ELF（现约 200KB）顶过内核 execve 的读取缓冲上限
  `EXEC_BUF_CAP`（`kunxcore/src/elf.rs`，256KB），导致 execve 拒绝、
  应用启动即退（实测复现过）；与系统字体一致走"rootfs 文件 + 运行时
  读取"，后续增长不再受此上限约束。Makefile rootfs 目标同步拷贝该文件。
- **launcher 接入**：图标瓦片缓存优先由烘焙贴图填充（半透明边缘的 alpha
  编码进颜色最高位字节）；`put_pixel` 支持逐像素 alpha 混合（屏幕/文件夹
  瓦片按目标现值混合，瓦片模式原样写入保留 alpha 供二次合成）；非 56px
  路径（Dock 52px、按压反馈缩放、占位页 96px）改为贴图最近邻缩放采样；
  撕裂淡出/编辑模式旋转/启动放大动画各路径透明像素跳过、半透明边缘混合。
  原"几何线条"绘制保留为贴图缺失时的回退。

### 重构：测试脚本统一归入 `tests/scripts/`（2026-09-05）

- **删除一次性调试脚本**（使命完成、仓库内零引用）：`tools/repro_calc.py`、
  `tools/repro_oobe.py`、`tools/verify_fixes.py`、`tools/verify_keys.py`、
  `tools/glass_verify.sh`；并清理测试运行日志（`tools/*.log`，本就
  gitignore）与 `__pycache__`。
- **迁移 18 个测试脚本** `tools/` → `tests/scripts/`：QEMU 端到端与回归
  （`apps_regression`、`lifecycle_test`、`lifecycle_core_test`、`power_test`、
  `camera_e2e_test`、`browser_test`、`cloud_e2e`、`kunx_pkg_test`、
  `launcher_test`、`weather_e2e`、`verify_files_app`、`devtool_verify`）、
  输出验证（`verify_jpeg`）、可视化探针（`app_open_shots`、`app_ui_probe`、
  `perf_harness`）及 `run_modemtest.sh`、`test_pdu.c`。Makefile 的
  `power-test` / `lifecycle-test` / `lifecycle-core-test` / `test-pdu` 目标
  与 `docs/`、`cloud/README.md` 中的路径已同步更新（历史 CHANGELOG 条目
  与 BUG_REPORT 中的旧路径保持原样，属当时记录）。
- **保留原处**：`tools/` 下构建/辅助工具（`mkdisk`、`mkfs_sfs`、`shot`、
  `input_inject`、`httpd` 等）；宿主测试 crate `tools/sfs_host_test` 与
  `tools/kunxverify`（`#[path]` 相对引用内核源码，移动会断链）；
  `tools/kunxsec`（签名工具链 Python 包，`selftest.py` 依赖同目录模块）。

### 修复：Contacts 启动即崩（SIGSEGV，栈溢出）（2026-09-04 轮）

**现象**：从桌面点击「联系人」→ `[crash] tombstone: pid=12 name=fork-child
sig=11 pc=0x4032bc`，应用退出码 245。pc 落在 `contacts::store` 的 Rust 栈
探测序言（`str xzr, [sp]` 逐页写哨兵）——写穿了 1MB 用户栈的守卫页。

**根因**：同步存储路径在栈上叠了三层巨型缓冲：调用方 `blob [u8; 262144]`
（256KB）+ `Store` 结构体（`recs[512]×256B` = 128KB，按值返回落在栈上）+
`Store::load`/`save` 的 `idx_buf [u8; 131088]`（128KB）。启动链
`k_main → merge_stores → store` 与 `k_main → rebuild_list(456KB) → store(369KB)`
叠加后超过 1MB，大帧函数的栈探测直接崩。大型栈帧 prologue（`sub x9, sp,
#imm, lsl #12` + 探测循环）在反汇编中逐函数核实。

**修复**（原则：巨型缓冲一律静态区，进程单线程顺序使用）：
- `apps/contacts`：`STORE_BLOB`（256KB）与 `STORE_SLOT`（`MaybeUninit<Store>`
  槽位）移到 static；`store()` 改为无参返回 `&'static mut Store<'static>`，
  6 个调用点删除本地 blob。`rebuild_list` 456KB→74KB、`store` 369KB→116KB，
  启动链峰值 ~350KB。
- `shared/sync/store.rs`：`load`/`save` 的 128KB `idx_buf` 移入模块级
  `IDX_SCRATCH` 暂存区（`idx_scratch()` 访问），所有 sync 应用共享受益。
- `apps/calendar`：`do_sync` 与首播路径的两处 256KB 局部 blob 复用静态
  `EV_BLOB`。`do_sync` 500KB→244KB，同步链峰值 ~590KB（含深子调用）。
- 审计 files / mail / notes：无 ≥256KB 大帧，无需改动。

### 修复：OOBE 首帧不再闪现像素字，全程平滑 TTF 汉字（2026-09-04 轮）

- 此前 OOBE 首帧以内置点阵兜底上屏后才同步加载字体（`load_fonts_deferred`），
  欢迎页先显示像素字、再重绘为平滑汉字，观感上「先像素后平滑」闪变一次。
- `apps/oobe/src/main.rs`：字体加载移回首帧**之前**——`load_fonts_sync()`
  在首次渲染前同步加载 OOBE 超小子集（oobe-cn/oobe-bold），首帧即平滑 TTF，
  向导 13 步自始至终不再出现点阵兜底闪现；仅字体加载失败时才回退点阵。
  依托 `shared/ttf_text.rs` 大块缓冲 brk 分配器，同步加载在 TCG 慢宿主下
  也只需秒级，不会重现「卡在启动画面」。

### 动效：桌面翻页 / 最近任务 / 锁屏转场 / Toast 四组动画对标 ColorOS 15（2026-09-04 轮）

launcher 侧此前四类界面变化为瞬切/静态，本轮补齐为完整动效（全部复用
`Spring`/`Easing` 运动库与现有渲染管线，规范数值已录入
`docs/animations.md` §3.7~3.10）：

- **桌面翻页动画**：左右滑动由瞬切改为 ColorOS 15 风格 —— 跟手段页面
  1:1 随手指平移（页界外 0.35x 橡皮筋阻尼），松手按位移（>1/4 屏）或
  速度（>600px/s）判定翻页，`Spring::page()`（ζ=0.8，≈260ms 近临界
  无过冲）吸附收敛；翻页时先提交目标页、从原相邻页位置弹入，内容视觉
  连续。当前页与相邻页经 `draw_page_at(page, offset)` 平移绘制（壁纸/
  状态栏/Dock 不动），页码点随进度平移并与目标页颜色交叉过渡。跟手/
  吸附期间帧预算升到 8ms（≈120Hz）。渐进式启动（BOOT_ACTIVE）期间
  保持原瞬切行为。
- **最近任务面板动效**：打开时卡片按槽位错峰 30ms 上滑入场（24px，
  200ms EaseOut，后台返回动画结束与应用退出码 2 统一经 `recents_open()`
  触发）；关闭改 150ms 下滑 + 压暗淡出（原瞬切）；新增单卡上滑清理 ——
  按住卡片上拖跟手，越过 -80px 后 200ms `FastOutLinearIn` 上飞出屏并
  从列表移除，其余卡片 220ms EaseOut 从旧格位插值补位，未达阈值回弹。
- **锁屏解锁/锁定转场**：解锁由瞬切改为锁屏整层（渐变底 + 时钟/键盘/
  图案内容）上滑出屏露出桌面（300ms FastOutSlowIn）；控制中心/远程
  「锁屏」改为整层从屏幕上方下滑入位（320ms）；滑动解锁支持跟手 ——
  层随手指 1:1 上移（下拖 0.15x 阻尼），越过 18% 屏高或速度足够即解锁，
  否则 250ms 回弹。转场期间锁屏输入全部屏蔽；`render_lock_screen` 拆为
  背景/内容/整层三函数（`draw_lock_background/content/layer`）供静态与
  转场复用，桌面绘制抽出 `draw_desktop_full()` 作为转场下层。
- **Toast 动效**：由静态 2s 改为「入场 180ms 底部上滑 16px + 淡入
  （FastOutSlowIn）→ 保持 → 退场前 150ms 淡出」。新增全局像素透明度
  `PIX_ALPHA`：`put_pixel` 写 BACK_BUF 时按其 alpha 混合（=255 零开销），
  玻璃药丸与文字整体淡入淡出，无需逐元素改绘。
- 顺带移除翻页/解锁后的「桌面页」「已解锁」Toast（动画本身即反馈，
  避免双重打扰）。

**验证**：`make launcher` 零警告；`make kernel` 全量编译 + ramdisk 重打
通过。手势优先级保持 控制中心 → 锁屏 → 翻页/最近任务 → 桌面，触摸与
鼠标（BTN_LEFT/BTN_TOUCH 合一路径）行为一致。

### 变更：全系统圆滑度对标 ColorOS 15——超椭圆连续曲率 + 4x4 超采样抗锯齿（2026-09-04 轮）

**背景**：此前全系统的圆角矩形 / 圆形都是硬边逐像素判定（`in_round_rect`
系整数圆弧测试），边缘是锯齿阶梯，观感「像素化」。本次把圆角几何升级为
ColorOS 15 同类的超椭圆连续曲率（squircle），并全面引入覆盖度抗锯齿。

**实现**：
- `shared/glass.rs` 新增 AA 核心：`rr_coverage`（4x4 = 16 超采样覆盖度）、
  `smooth_fill_rr` / `smooth_stroke_rr`（抗锯齿超椭圆圆角矩形填充 / 描边，
  内部直线段走整像素快路径，仅四个 r×r 圆角盒逐像素混合）、
  `smooth_fill_circle` / `smooth_stroke_circle`（抗锯齿实心圆 / 1px 圆环）、
  `SmoothTarget` trait。超椭圆隐函数 `(1-k)·(qx²+qy²)/r² + k·(qx⁴+qy⁴)/r⁴ ≤ 1`，
  k 随半径 0→24px 从正圆弧渐变到 n=4 超椭圆（只用整数次幂，no_std 可用）。
- `in_round_rect` 切换到同一超椭圆几何（命中区与视觉一致）；毛玻璃
  `glass_rect_style` / `glass_rect_solid` 裁剪与描边升级为覆盖度 AA
  （直边描边保持锐利 1px，圆角段随曲线淡出）；`blend` 增加不透明快路径。
- 接入点：launcher（图标 / 文件夹 / 小组件毛玻璃 / 控制中心 / 锁屏图案）、
  settings、oobe（含 widgets/button）、clock、camera（快门圆盘 / 圆环）、
  browser（裁剪圆点）、app_kit（`aa_fill_round_rect` / `aa_stroke_round_rect`
  / `aa_fill_circle` / `draw_disc` / `draw_circle` / `blend_put`）、kunxui
  （Canvas 默认 `fill_rounded` / `rounded_border` / `fill_circle` / `draw_circle`
  经 `#[path]` 复用 glass.rs，零第二份实现）。
- 圆角取值对标 ColorOS 15（540x960 屏）：桌面图标圆角比例 25%→**30%**
  （56px→17px）；玻璃按钮 / OOBE 按钮全胶囊（h/2）或 30% 比例（上限 16px）；
  卡片 / 面板统一 16px（等比 1080p 的 32px）；键盘按键 25%→30%；settings
  图标底座 6→7px（30% 比例）；oobe 卡片 18→16、输入框 14→16。

### 变更：系统鼠标指针换为 tubiao/shubiao.png 贴图（2026-09-04 轮）

**背景**：此前系统鼠标指针是各处硬编码的 9x9 位图箭头（黑描边 + 白填充），
散落在 7 个文件里（`shared/cursor_overlay.rs`、`shared/app_kit.rs`、launcher、
oobe、calculator、store、stub 各有一份拷贝），无法定制外观。

**实现**：
- 新增 `shared/cursor_sprite.rs`：指针贴图单一来源。`tools/make_cursor.py`
  把 `tubiao/shubiao.png`（256x256 RGBA）面积平均缩放为 24x24 RGBA8888
  写入 `shared/cursor.rgba`（仅标准库 zlib，无需第三方依赖），由
  cursor_sprite `include_bytes!` 编译期内嵌；热点取源图尖端 (56,16) 的等比
  映射 (5,2)。像素混合从「描边+实心覆写」改为逐像素 alpha 混合，支持
  launcher 亮度 LUT（`draw_at(..., Some(apply_lut))`）。
- 各绘制点统一改为经 `cursor_sprite::draw_at` 绘制；光标备份/脏区从
  11x11 扩为 26x26（贴图 24x24 + 1px 边距），`CUR_UNDER` 及 launcher/oobe
  的 `mark_dirty`/`present_rect` 矩形同步放大，旧位置恢复不再有残影。
- 删除 calculator/store/stub/oobe 里无调用的死代码 9x9 位图拷贝
  （真实路径早已全部走 overlay）。
- Makefile：`SHARED_SRC` 加入 `cursor_sprite.rs` 与 `cursor.rgba`，并新增
  规则 `shared/cursor.rgba: tubiao/shubiao.png tools/make_cursor.py`——
  换指针图只需替换源图后重跑 `make`，所有 GUI 应用自动重编生效。

### 修复：QEMU 下全面屏手势失效（virtio-tablet 点击以 BTN_LEFT 上报）（2026-09-02 轮）

**现象**：`make run-gui` / `run-phone`（QEMU + virtio-tablet-device）里用鼠标拖拽
无法触发全面屏手势 —— 底部上滑不回桌面、边缘内滑不返回，只有点击有效。

**根因**（对照 QEMU 源码 hw/input/virtio-input-hid.c 确认，非推测）：
- QEMU 的 virtio-tablet-device 是**绝对定位鼠标**：宿主左键点击经 `keymap_button`
  映射为 **BTN_LEFT**（0x110）下发，`BTN_TOUCH`（0x14a）只出现在设备能力位图、
  实际点击并不产生。
- launcher 与 app_kit 此前只在 **BTN_TOUCH** 路径做完整手势识别（`finish_touch` /
  `classify_swipe`：底部上滑、边缘返回、滑动、长按）；**BTN_LEFT 只映射为 Tap**，
  因而 QEMU GUI 下全面屏手势完全失效（真机触摸面板经 UART 协议帧 → BTN_TOUCH，
  不受影响）。

**修复**：
- `apps/launcher/src/main.rs`：BTN_LEFT 按下与抬起**并入与 BTN_TOUCH 完全相同
  的手势识别路径** —— 按下记录 `TOUCH`（press 坐标/事件时间戳），拖动期间在
  EV_REL / EV_ABS 同步推进手势轨迹（cur/max_dist），抬起走 `finish_touch()`：
  导航手势（三键点击 / 底部上滑 / 边缘返回）优先，其次点击 / 滑动 / 长按。
- `shared/app_kit.rs`：应用内同一修复 —— BTN_LEFT 按下记录 `TOUCH_DOWN/PX/PY/US`，
  抬起按导航栏点击 → 全面屏手势 → 点击 顺序分类，QEMU 下应用内上滑回桌面可用。
- 长按吞点击（`MOUSE_LONG`）、控制中心滑块拖动吞噬逻辑保持不变；QEMU 与真机
  从同一套手势判定，行为一致。

**验证**：`make kernel` 全量编译通过（launcher/kernel 重编并重打包 ramdisk）。
TCG `-display none` 串口注入路径（BTN_TOUCH）与真机不受影响；GUI 鼠标路径
（BTN_LEFT）已与触摸路径合并为同一识别逻辑（`shared/nav.rs::classify_swipe`）。

### 修复：TCG 宿主「开机卡死、进不了系统」+ 起动路径 TTF 平滑字（2026-08-30 轮）

**现象**：QEMU（x86 宿主 → 强制 TCG）下串口只有 sensord/logd/positiond 的周期
日志循环刷屏，OOBE 向导永不出现、桌面永不呈现、内核 Shell 也不接管。

**根因**（QEMU + 插桩实测确认，非推测）：
- 启动链中 OOBE 的 `k_main` 第一步同步调用 `ttf_text::font_init()`：fontdue 解析
  两个 ~1.6MB TTF 子集时，用户态 bump 分配器**每页做一次 brk 系统调用**（实测
  ~15765 页/字体），每次 brk 在内核里逐页分配/清零/插页表/失效 TLB。TCG 下
  单字体解析实测 **~30s**（宿主原生仅 23ms），两个字体 ~60s；期间 init 阻塞在
  `wait4(oobe)`，launcher 永不启动，hence「永远卡住」。
- 低配 TCG 宿主上该 ~128MB 堆增长还可能把整个客户机拖入长时间停滞（所有守护
  进程周期日志一起消失），进一步掩盖问题。

**修复**：
- `shared/ttf_text.rs`：brk bump 分配器改为**大块缓存 bump**——逻辑游标与内核
  映射断点分离，余量内分配零系统调用，仅余量耗尽时一次 brk 拉入整块（4MB）。
  fontdue 解析期间 ~3 万次逐页 brk 降为 ~几十次，两个字体解析在正常宿主上
  秒级完成；bump 语义不变，全量消费无浪费。**全系统（OOBE + launcher + 全部
  桌面应用）平滑 TTF 字体在此得以保留并按需快速加载（正文 = fonts/uiputong.ttf
  子集 cn.ttf，标题 = fonts/bold.ttf 子集）。**
- `apps/oobe/src/main.rs`：字体加载从启动路径**移到首帧之后**——先用内置点阵
  兜底把欢迎页渲染上屏（用户立即看到向导，消除「卡在启动画面」的感觉），随后
  同步 `font_init()`（配合大块缓冲 brk，TCG 下秒级），完成后重绘当前步并把
  向导全部 13 步文字切到平滑 TTF（正文 uiputong / 标题 bold）。
- `apps/launcher/src/main.rs`：恢复 `font_init()`（桌面使用 cn.ttf + bold.ttf
  平滑文字），依靠新的块缓冲分配器在慢宿主下也只需秒级；字体未就绪时仍回退
  内置点阵，桌面可靠呈现。

**验证**（QEMU TCG，x86 宿主）：
- 宿主基准：cn.ttf/bold.ttf 各 ~23ms 原生解析通过（字体内容正常）。
- `-nographic`（make run）：OOBE 秒级退出（无帧缓冲 → WARNING → launcher 退出）
  → 内核 Shell `kunxos$` 正常接管，guest 持续健康运行不再全局停滞。
- ramfb GUI：OOBE 即时映射 540×960 帧缓冲、欢迎页立刻上屏（点阵兜底，瞬时）；
  随后 fontdue 解析平滑字体（正文 uiputong / 标题 bold），完成后整屏重绘为
  平滑字；30s 无输入自动推进向导。桌面随用户完成后以平滑字启动。
- 说明：极端重度负载的 TCG 宿主上 ~128MB 字体堆增长可能让客户机短时停滞
  （两次 allocator 变体均出现，属模拟环境载荷问题而非逻辑死锁；你的宿主日志
  持续推进证实正常宿主不受影响）；真机/KVM 宿主为瞬时完成。

### 样式与动效：OOBE 首次开机向导全面对标小米澎湃3（2026-08-28 轮）

功能保持不变（13 步流程、配置落盘格式、输入/模拟点击回退语义均不动），
仅视觉样式与交互动效整体对齐澎湃3 新机设置向导：

- **澎湃3 配色**：纯白渐变底（#F9FBFD → #E9F1F4）+ 近黑正文（#14161A）+
  澎湃 teal 高亮（#10B89E 主色 / #0C9682 深色 / #E6F8F4 选中底）；
  底部导航面板改为白色半透液态玻璃（中性灰描边），按钮主色由 iOS 蓝改为
  澎湃近黑 CTA（#17171B），按压时轻微提亮（#2C2D33）并 0.96 缩放回弹。
- **版式重构**（`apps/oobe/src/main.rs`）：语言/时区/锁屏/胁迫动作/外观一律
  使用白色圆角卡片（492×54，r=18）+ 右侧 teal 单选圈；模块/网络/协议改为
  右侧 teal 圆角复选方框（内带白色对勾）；输入框白底圆角 + 聚焦 teal 描边 +
  淡 teal 底；外观步骤带浅/深色预览圆；完成页新增 teal 对勾圆盘 + 摘要白卡；
  欢迎页新增渐变 teal Hero 徽标（K 字）+ 标语；底部按钮欢迎页为通栏
  「开始使用」，其余步骤为返回芯片 + 主按钮组合。
- **动效引擎**（`transition_pages` / `draw_nav_anim` / `animate_welcome` /
  `pulse_selection`，260ms @ 16ms 帧）：
  - 步间转场：旧页交叉淡化 + 新页上滑 16px（平滑阶梯 EaseInEaseOut），
    底部面板每帧重绘、进度圆点滚动放大；
  - 欢迎页：Hero 徽标缩放淡入（14 帧）+ 标题/副标题上滑淡入（12 帧）；
  - 选中卡片：teal 高亮脉冲淡出（≈110ms）；
  - `finish.rs`：桌面壁纸从白屏淡入 12 帧，「进入系统」进度条改为澎湃 teal。
- 按钮组件（`apps/oobe/src/widgets/button.rs`）：新增通栏按钮 `draw_full`、
  按压 0.96 缩放与回弹、主按钮最小宽度 176，主按钮按压提亮而不是变暗。
- **OOBE 无法进入的两个根因修复**（QEMU 实机验证）：
  - `MODULES` 新增到 21 项后 `MODULE_ON` 仍为 `[bool; 20]`，启动初始化
    `MODULE_ON[i] = MODULES[i].default_on` 越界 panic → 改为 `[bool; 21]`；
  - OOBE 直写 `/dev/fb0` 映射（合成器 canvas）后从未提交
    `FBIO_PUT_DAMAGE`/`FBIO_PRESENT`，屏幕始终停留在 bootloader 启动画面
    （用户看到「两个大方块」）→ 新增 `present_fb()`：保留 fb0 fd，每帧
    `flip()`/转场帧/选中脉冲后提交整屏脏区并请求合成器翻页；
  - `font_init` 在 QEMU TCG 下对 3MB cn.ttf 的 fontdue 解析同步卡死
    （宿主机解析 46ms，客户机无限旋转）→ OOBE 改为直接使用内置 CJK
    点阵字库（text 管线在 `font_ready()==false` 时自动回退），不再阻塞。
- 验证：`make kernel` 通过；QEMU 实机截图确认向导上屏（背景 #F9FBFD、
  teal 高亮 #10B89E、近黑主按钮 #17171B、浅色底部面板）。

### 修复：OOBE 缺字与指针不跟随（2026-08-29 轮）

- **缺字**：`shared/cjk_font_oobe.rs` 是按旧源码生成的 GB2312 子集，
  新文案（胁迫/伪装/警报/外观等）中不在子集内的字只推进宽度不绘制
  → 用 `tools/gen_cjk_font.py subset` 依当前 `main.rs`/`finish.rs`/
  `widgets/button.rs` 重新生成（689 字形，覆盖源码全部 690 个 CJK 字符）。
- **指针（鼠标）不跟随/不可见**：光标画进合成器 canvas 后未触发
  `FBIO_PRESENT`，屏幕上看不到指针移动 → `cursor_overlay_init` 与
  `move_cursor_fb` 结束后统一 `present_fb()`；`ABS_X/ABS_Y/ABS_MT`
  坐标补 `clamp` 到屏幕范围，触摸板/平板绝对坐标不再越界。
- 顺带补齐并发重构遗留的机械编译错误（非本次功能改动）：
  `linux_compat.rs`/`process.rs` 的 `pipe_drop_ref` 增补 `is_write`
  参数、`FD_WRITE` 改引 `crate::ipc::FD_WRITE`；launcher 内置点阵
  字形像素坐标补 `as i32` 类型转换 —— `make kernel` 恢复通过。
- 验证：QEMU 实机截图确认屏幕中心 (270,480) 呈现代理指针（白描边黑）、
  标题/锁核心模块行正常渲染，向导按步推进无空白缺字。

### 性能与动效：OOBE 卡顿治理 + 常驻动画（2026-08-29 轮）

**卡顿根因**：光标/微动画每帧都提交「整屏 540×960 脏矩形」→ 合成器每次
canvas→back 整屏 2MB 拷贝 + 合成 + 翻页；QEMU TCG 下鼠标每动一次都触发，
拖动明显掉帧。

- 新增 `present_rect(x,y,w,h)`（脏矩形呈现）：
  - 光标移动/重绘 → 旧+新位置并集小矩形（≤14×14）；
  - 选中脉冲 → 卡片矩形；按钮回弹 → 导航条矩形（540×84）；
  - Hero/进度点呼吸 → 各自小区域。
- **常驻动画**（等待输入期间不再静止，对标澎湃3 活页感）：
  - 欢迎页 Hero 徽标持续呼吸缩放（0.975..1.025，周期 ≈640ms）；
  - 其余页当前进度点呼吸脉动（半径 4..5，周期 ≈380ms，玻璃底快照恢复）；
  - 卡片「进入悬浮态」：语言/时区/锁屏/胁迫动作步骤，指针悬停卡片呈
    teal 描边高亮，离开恢复（同页脏矩形重绘）；
  - 按钮松开回弹只重绘底部导航条（`btn_tick_nav_only`），不再整屏渲染。
- 验证：QEMU 实机两次 0.25s 间隔截图对比 —— 进度点带区域 340 像素变化
  （呼吸在走），页面/桌面正常呈现。

### 修复：OOBE 后的「两个大方块」观感（2026-08-29 轮）

排查：OOBE 全程正常、向导页渲染正确；「两个大方块」出现在 OOBE 完成后
launcher 锁屏阶段 —— 根因有二：

- OOBE 默认外观是**深色主题**（`THEME_DARK` 初值/默认推进均为 true，壁纸 2），
  launcher 桌面=深色壁纸；launcher 锁屏背景又是**硬编码深蓝渐变**
  （#101527→#1B2436）+ 按键玻璃块，整体一片深色，用户观感就是「两块」。
- 修复：
  - `apps/oobe/src/main.rs`：`THEME_DARK` 默认与默认推进改为 `false`（浅色），
    `WALLPAPER=1`（浅色 teal 壁纸）；
  - `apps/launcher/src/main.rs`：锁屏背景随主题（浅色=浅灰蓝渐变
    #F7FAFD→#E7EFF7），时钟/日期/提示/指示点/按键文字颜色随主题
    （浅色用深字 `#14161A`/灰 `#6B7076`），按键玻璃本身已随主题自适应。
- 验证：QEMU 实机截屏 —— 锁屏上部为浅色渐变+深色大时钟/日期/teal 锁图标，
  下部为浅玻璃数字键盘（深色数字可见），不再是深色大方块；OOBE → 桌面
  全程渲染正常。

### 修复：OOBE 后进不了系统（默认 PIN 锁屏挡路）（2026-08-29 轮）

**根因**：`rootfs/etc/oobe_default` 默认配置自带 `security_type`（pin）与
`security_hash`（默认 1234），首启迁移进 `/data/oobe` 后 launcher 读到即
自动上锁（`[lock] screen locked at boot`）——用户没设过密码却被锁屏挡住，
表现为「进不了系统」；该目录 theme 也是 `dark,wallpaper:2`。

修复：
- `apps/oobe/src/main.rs`：新增 `SEC_ENABLED`，默认（无交互推进/未选锁屏
  方式）为 false —— `save_security_type/save_password(security_hash)/
  save_duress/save_duress_action` 仅在启用安全时写文件；点击锁屏方式卡片
  才置位。用户主动设置安全时功能完全保留。
- `rootfs/etc/oobe_default`：移除 `security_type`/`security_hash`/
  `duress_action`/`duress_hash`（不再迁移默认锁）；`theme` 改为
  `light,wallpaper:1`（浅色 teal）。
- 验证：QEMU 实测一轮 OOBE → launcher 启动日志**无** `[lock] screen
  locked`；截屏为浅色 teal 桌面（壁纸渐变 + 顶部小组件 + 底部 Dock 图标），
  可直接进入系统。
  `[lock] screen locked`；截屏为浅色 teal 桌面（壁纸渐变 + 顶部小组件 +
  底部 Dock 图标），可直接进入系统。

### 性能：OOBE 交互卡顿治理（2026-08-29 轮）

剩余卡顿来源：每次点击/打字都走「整屏 render + flip（2MB）+ 全屏呈现
（2MB）」，且转场/入场帧数多、帧间还 sleep。

- 点击选项改为**局部重绘**（`partial_redraw`）：卡片页（语言/时区/锁屏/
  胁迫动作/外观）只重绘选中变化的旧/新两张卡片并呈现小矩形；模块/网络/
  协议复选只重绘复选框；不再整屏 render + 4MB 搬移。外观卡片局部重绘
  补齐颜色预览圆（`draw_theme_swatch`）。
- 转场：`TRANS_FRAMES` 15→8，帧间不再 `sleep_ms(16)`（TCG 下每帧本就
  含 47 万像素合成 + 全屏呈现，越少帧越顺）；欢迎页入场 26→19 帧；选中
  脉冲 7→6 帧。
- 验证：QEMU 实测 12 步全部走完 → launcher 正常、无自动锁屏日志。

### 新增：图形终端 / TTY / PTY 子系统（Phase 21）（2026-08-28 轮）

- **内核 TTY 会话**（`kunxcore/src/tty.rs`）：`/dev/tty1..4` 会话终端 +
  `/dev/ptmx` 分配器；in_q/out_q 环形缓冲（4096B / 16KB）、`attached`/
  `overflow`/窗口尺寸；输出镜像（`uart_send_char` → 活动会话 out_q）与
  输入路由（Shell 会话附接时只读 in_q，未附接回退 UART）均在内核 DAIF
  临界区内完成。
- **PTY 主从模型**：图形终端（EL0）为 PTY 主侧，内核 Shell（EL1，
  `shell_main` 内核进程）绑定会话 0 为从侧 —— 等价于「GUI 终端 fork shell
  子进程」；`/dev/ptmx` 预留多会话（TIOCGPTN 返回从侧号）。
- **Shell 行编辑增强**（`shell.rs::read_line`）：光标左右移动（ESC[D/C）、
  Ctrl+A/E（行首/行尾）、行中插入/删除、Ctrl+U/W（删至行尾/删前一词）、
  历史 ↑↓ 浏览、Tab 补全；Ctrl+C/D/Z 信号（SIGINT/SIGTERM/SIGTSTP，面向
  前台 `run`/`busybox` 子进程）；提示符取 `$PS1`。
- **新内置命令**：`alias` / `unalias`（定义/查询/删除别名）、`source`
  （脚本逐行执行，跳过空行与 `#` 注释，命令词先做别名展开）——简单脚本
  支持，配合既有管道 `|`、重定向 `< > >>` 与环境变量（`export`）。
- **图形终端应用**（`apps/terminal/src/main.rs` 重写）：玻璃标题栏 + 文本区 +
  滚动条 + 系统导航栏；精简 VT 仿真器（96×60 网格；ESC[A/B/C/D/H/f/G/d 光标、
  ESC[J/K 清屏、SGR 16 色 + 粗体、`\r\n\t\b\f` 控制字符、DEC 私有序列忽略）；
  24 行回看 + 触摸拖动滚动 + 滚动条滑块；CJK/ASCII 混排（复用 ttf_text，
  提示词 #11 文本渲染）；光标块闪烁；输入转发（方向键 CSI、Ctrl+字母控制
  字节、Shift 大写）——哑终端不本地回显。
- 设计文档 `docs/terminal.md`（关键问题确认：Shell 内建于内核；图形终端走
  合成器 `/dev/fb0` 持久画布 + FBIO_PRESENT 脏区上屏）。
- 验证：内核 `make kernel` 与终端应用 `make terminal` 均编译通过。

### 新增：应用生命周期与后台管理系统（lifd / pushd / launcher 集成 / 状态持久化）（2026-08-28 轮）

- **生命周期状态机**（`kunxserv/lifd`，init 在 launcher 之前拉起）：Running（前台、
  nice −3）→ Background（SIGSTOP 冻结、内存保留）→ Suspended（后台超时/内存压力，
  LMK 首选）→ Terminated（LMK/用户终止）。launcher 以非阻塞 `wait4(WNOHANG)` 轮询 +
  帧快照/返回与恢复动画配合，切应用不再"杀旧重启"。
- **控制面**：`/data/life` 文件信箱（`reg_*` 登记 / `cmd_<pid>` 命令 / `evt_<pid>`
  事件 / `launcher_evt` / `recents`），单行 ASCII、最新覆盖语义，可 cat 调试、可注入。
- **应用侧 API**（`shared/lifecycle.rs`，经 app_kit 暴露）：`on_pause`/`on_resume`/
  `on_suspend`/`on_terminate`/`on_notify` 回调、`lc_home`/`lc_go_recents`（后台化并
  阻塞至恢复）、`lc_request_bg_task`/`lc_end_bg_task`、`lc_save_state`/`lc_restore_state`
  （/data/state/<app>.bin，SFS 持久）、`lc_notify_local`。应用主循环把
  `exit(nav_exit_code(ev.nav))` 换成 `lc_handle_nav(ev.nav)` 即获后台化能力。
- **后台任务白名单**（lifd 审核）：Audio/Location/Voip/Download 四类 + 时长上限
  （3600/1800/7200/1800s）+ 每应用 2 个 + 全系统 3 个上限；超时发 `BG_TASK_EXPIRED`，
  持有任务的应用不因超时转入 Suspended。
- **内存压力 / LMK**：内核 `mem_pressure` 模块（tick 100ms 计算
  Normal/Moderate/Critical；`/proc/pressure` 支持 `force <0|1|2>`/`clear` 测试注入），
  lifd 按"状态优先 + LRU 老化 + 后台任务罚分"评分升序击杀；`SYS_LC_MEMPRESSURE(404)`
  读取等级。
- **内核配合**（不替换 CFS）：`TaskState::Stopped` + `stop_current`/`cont_pid`，
  SIGSTOP=停 / SIGCONT=继续（原 SIGSTOP=杀已改）；`setpriority` 真实实现（root 可改
  任意 pid nice → CFS 权重：前台 −3 / 后台 +2）；`wait4` 支持 WNOHANG。
- **推送框架**（`kunxserv/pushd`，init 拉起）：本地通知队列（/data/push/in →
  `DELIVER` → lifd 按状态投递：前台立即、后台/挂起延迟补投并在恢复时随
  `RESUME+NOTIFY` 合并事件送达、未登记丢弃）；设备 token 注册与远程推送通道接口预留
  （APNS/FCM 等价物占位）。
- **演示/测试载体**：`apps/lcapp`（注册全部回调、状态保存/恢复、后台任务与本地通知
  按钮、串口标记供自动化断言），launcher 桌面页面 0 图标；文档
  `docs/lifecycle.md`（设计 + 关键问题回答）+ `docs/lifecycle-test.md`（测试方案）；
  自动化 `tools/lifecycle_test.py`（QEMU，`make lifecycle-test`）。
- **修复**（本路径联调中发现）：lifd `DELIVER` 载荷解析丢失 id/payload（改从原始串
  解析）、恢复补投由 append 改为"最新覆盖"写、`lc_poll` 解析 `RESUME+NOTIFY` 合并事件
  并触发 `on_notify`、lcapp 加入 launcher 页面 0 布局（此前不可达）、`lc_set_name`
  固定状态文件名。

### 修复：SFS v2 并发缺陷（inode/块串线 + 事务提交除零 panic）（2026-08-27 轮）

- **根因**：SFS v2 的事务状态（`TXN_DEPTH/TXN_MAJOR/PENDING_*`）与 RAM 位图是
  内核全局静态量；syscall 在 EL1 处理期间放开 IRQ（可被 10ms 定时器抢占），
  多任务并发进入 FS 时事务被交错污染、分配位图「读-改-写」发生竞态。
  - 症状 A：多进程并发在 `/var/run` 创建文件时目录项/inode/数据块张冠李戴
    （camerad.ctl 读到 audiod.ctl 等其它文件的数据，反复出现）。
  - 症状 B：事务状态被污染为 `major=u32::MAX` 后，`txn_commit` 在 `jlog_place`
    对未挂载伪设备（`jlog_blocks==0`）取模 → 除零 panic，随机崩溃守护进程。
- **修复**（`kunxcore/src/fs_simple.rs`）：
  - 新增**全局递归 SFS 锁** `fs_lock/fs_unlock`（按持有任务递归；等待者让出 CPU、
    不屏蔽 IRQ、无死锁）；全部 22 个对外接口包锁并拆 `_unlocked` 内部实现，
    使每个顶层文件系统操作成为原子段 —— inode 唯一、块不重叠、事务独占。
  - `jlog_place`/`txn_begin`/`commit_inner` 对 `jlog_blocks==0` 防御（返回失败
    并按「未提交事务整体丢弃」回滚），根除取模除零。
  - 锁与既有 WAL/掉电保护正交：事务协议、双超级块、恢复重放、fsync 语义不变。
- **验证**：新增多进程并发回归测试 `apps/sfsrace`（6 子进程在 `/var/run`
  并发 create→写跨 3 块唯一载荷→读回逐字节校验→truncate→删，任何串线/崩溃即
  FAIL）；`make test-sfsrace` 无头 QEMU 断言 `SFS-RACE PASS`（init 以
  `/sfsrace-test` 标记触发并自动关机）。另有**宿主多线程决定性验证**
  （`tools/sfs_host_test -- race`）：锁桩失效时完整复现串线/截断残留/孤儿块/
  悬空目录项（fsck ≥10 项错误），锁桩改为全局唯一 tid 后同一负载 0 失败且
  fsck 全绿、`test_lock_serialization` 测得临界区最大并发 = 1。详细文档见
  `docs/sfs-v2-concurrency.md`。

### 新增：相机子系统（HAL / camerad / 相机应用 / 纯 Rust JPEG / 3A 框架）（2026-08-27 轮）

- **相机 HAL**（`kunxhal/src/camera.rs`）：`CameraDevice` trait（枚举/开闭/
  配置/起停采集/单帧拍照/3A/按缓冲索引读帧）、`CaptureRequest`/`CaptureResult`
  （时间戳与 AE/ISO/AWB/对焦元数据）、`BufferPool<N>`（CPU/DMA 双用缓冲槽，
  `set_phys` 为 Pi4 Unicam DMA 零拷贝预留）、`PixelFormat`（RAW8/YUV420/RGB888）。
- **平台后端**：`SimCamera`（QEMU 软件传感器：彩条 + 帧计数条 + 时间戳条 +
  运动块，帧按 (seq, ts) 确定性渲染，零大缓冲）；`PiCamera`（Pi4 骨架：
  open/configure/3A 状态机真实生效，采集路径返回 NotImplemented，Unicam/BSC/
  IMX219 寄存器常量与接线序列已整理在 `kunxhal/src/plat_pi4.rs`）。
- **3A 框架**：`ThreeA` trait + `FrameStats`（亮度/对焦评价/通道均值），
  默认 `FixedThreeA`（1/30s、ISO100、5500K、对焦 50）；AE 比例步进、AWB 灰世界、
  AF 爬山留接口。
- **相机服务**（`kunxserv/camerad`，init 拉起）：文件命令通道 + 环形预览帧
  （/tmp/cam 固定 6 文件复用）+ 拍照 JPEG 落盘（/data/photos）+ 录像自描述容器
  （/data/videos/rec_*.kxv，KXRV 头 + 每帧时间戳；MP4/MOV 预留）。
- **JPEG 编码器**（`shared/jpeg`，纯 Rust、no_std、零堆）：RGB888 → JFIF
  baseline 4:2:0，整数 DCT + 标准量化/Huffman 表；7 个宿主单测 + Pillow 视觉
  校验（`tools/verify_jpeg.py`）。未绑定 libjpeg-turbo（目标无动态链接）。
- **相机应用**（`apps/camera`）：取景器（YUV→RGB 最近邻缩放 + 三分构图线）、
  快门闪光 + 处理进度、REC 计时、缩略图、camerad 缺失回退模拟取景器。
- **测试**：kunxhal 相机 HAL 6 个宿主单测（BufferPool/SimCamera 确定性/3A/
  枚举/单帧）、`tools/camera_e2e_test.py`（QEMU 端到端）。
- **已知阻塞**（既有 SFS v2 内核写路径缺陷，非本次引入）：多进程并发在
  /var/run 创建文件出现 inode 串线、事务写触发 `jlog_place` 除零 panic，
  QEMU 端到端（含其它守护进程的文件写）需先修复内核 SFS（见
  `docs/camera.md` §8.2）。已修复本次新增的具体问题：Pi4 构建下
  `camera_backend()` 双 cfg 块编译错误、录像容器 KXRV 头每帧重复写入。

### 新增：开发者工具链（GDB 调试桩 / tombstone / 结构化日志 / 性能 overlay / 调试 shell 工具）（2025-08-27 轮）

- **GDB 远程调试桩升级**（`kunxcore/src/gdbstub.rs`）：
  - 真实软件断点 `Z0`/`z0`：写入 BRK #0，命中后「摘除 → 硬件单步跳过 → 静默重插」
    （标准 gdb 断点舞蹈）；指令缓存维护（dc cvau / ic ivau）。
  - 真实硬件单步 `s`：MDSCR_EL1.SS，步进异常后停回 GDB；断点处继续 = 摘除→步进→重插。
  - EL0（BRK64 EC=0x3C / 步进 0x32）与 EL1（内核断点 EC=0x3C / 0x33）均可命中，
    陷阱帧/PANIC_REGS 与 gdb 寄存器集互拷，`g`/`G`/`p`/`P` 写后继续生效。
  - 高级功能（线程 `Hc`/`Hg`、watchpoint `Z2`、硬件断点 `Z1`）预留（空响应）。
- **崩溃报告（tombstone 等价物）**（`kunxcore/src/crash.rs`）：
  - EL0 用户故障（SIGSEGV/SIGILL/SIGTRAP/SIGBUS，由 ESR.EC 解码）与内核
    panic/同步异常统一生成结构化报告：时间/进程/信号、寄存器、ESR/FAR 解码、
    内存映射（`user_pages` 按权限聚合）、用户栈回溯（x29 帧指针链 + 栈转储）、
    内核日志尾部。
  - 落盘 `/data/crash/tombstone_<ts>_pid<pid>.txt`（无 /data 回退 /tmp/crash），
    保留最近 8 份；VFS 不可用（早期 panic）仅输出控制台。
  - shell 命令：`crash`（列表）/ `crash show <file>` / `crash clear`。
  - 新增崩溃注入测试程序 `apps/crashme`（segv / ill / div0）。
- **结构化日志（dmesg / logcat 等价物）**（`kunxcore/src/log.rs`）：
  - `printk!(LEVEL, tag, ...)`：级别（0-7）+ 模块标签 + 时间戳，写入结构化
    环形缓冲（256 条）并镜像 UART 控制台；关键启动里程碑已接入。
  - `/dev/kmsg` 字符设备（major 11, minor 1）：read = 增量取记录；write =
    注入用户态日志（`<level>[tag] msg`）并镜像控制台 —— 应用日志的 logcat 通道。
  - `kunxserv/logd` 用户态守护：读 /dev/kmsg 持久化到 `/data/log/logcat`
    （无 /data 回退 /tmp/log；256KB 环形截断），init 拉起。
  - shell 命令：`log [-l <level>] [tag]`（过滤视图）；`dmesg` 保持原始控制台缓冲。
- **性能统计与 overlay**（`kunxcore/src/perf.rs`）：
  - FPS（合成器 present 计数，1s 窗口）、CPU%（调度 tick idle/忙比例）、
    MEM%（物理页分配器）。`perf on` 在屏幕右上角绘制统计
    （8x8 点阵放大 2x + 半透明黑底），随每帧合成上屏，开销 <1ms。
  - 调度器新增每任务 `cpu_ticks` 记账；`top` 按累计 tick 排序显示
    PID/TID/状态/nice/tick/CPU 上限。
  - shell 命令：`perf on|off|stats`、`top [-n 1]`。
- **调试 shell 工具**：新增 `mount`（列出挂载点 + 块设备，`mount <dev> <point>`
  尽力挂载）、`ifconfig`（IP/掩码/网关/DNS/MAC）、`log`、`crash`、`perf`、`top`；
  内置 shell 现随 /init 并行拉起（UART 调试接口始终可用）。
- **文档**（`docs/DEBUGGING.md`）：QEMU GDB 连接（串口 TCP 转发与 QEMU 内置
  gdbstub 两路）、用户进程断点/单步流程、Pi4 串口（PL011/miniuart + JTAG 说明）、
  KVM/HVF 加速的宿主架构约束、tombstone/log/overlay 使用速查。
- **验证工具**：`tools/devtool_verify.py`（串口 TCP 自动验证 shell 工具 /
  crash / log / perf）；`make run-debug` 等价 QEMU 命令。

### 新增：OOBE 分步安全设置（用户名 / 锁屏方式 / 密码+确认 / 胁迫密码+确认 / 胁迫动作）（2025-08-26 轮）

- **OOBE 步骤重构（10 步 → 13 步）**（`apps/oobe`）：按用户反馈把「账号 +
  密码 + 胁迫」混在一页拆成独立步骤：
  1. 账户步骤只设**用户名**（云服务 / 家长控制复选框保留）
  2. 「锁屏方式」：PIN / 图案 / 密码单选
  3. 「锁屏密码」：密码 + 二次确认（掩码，校验 4-16 位且两次一致）
  4. 「胁迫密码」：胁迫密码 + 二次确认（校验与锁屏密码不同）
  5. 「胁迫动作」：伪装系统 / 静默擦除 / 发送警报
  6. 主题 → 完成（完成页摘要显示锁屏方式与胁迫动作）
- **胁迫密码真正可配置**：此前 `DURESS_PIN` 硬编码 9999、OOBE 没有任何输入
  框，用户在锁屏填自己的胁迫密码自然无效。现在胁迫密码由用户在 OOBE 设置，
  `duress_hash` 保存其 FNV-1a 哈希，锁屏输入即触发对应动作。
- **修复保存时序 bug**：`apply_step_defaults` 在 `save_step` 之前执行，若在
  其中重置密码缓冲会覆盖用户刚输入的内容（保存错误哈希）；密码/胁迫密码
  的默认值改为 k_main 预填（1234 / 9999，可编辑），步骤完成时不再重置。
- **云令牌时序修复**：`cloud_account` 令牌（fnv1a64(用户名:密码:盐)）原在
  账户步骤生成（此时密码还是默认值），改为密码步骤生成，使用最终密码。

### 修复：锁屏「设完密码填进去提示密码错误」（任意长度密码解锁）（2025-08-26 轮）

- **根因**：锁屏 PIN 校验在输满 4 位即判失败并清空输入 —— OOBE 设置的
  6 位及以上密码永远输不完（第 4 位就被重置），表现为「填进去提示密码错误」，
  也导致 OOBE 完成后卡在锁屏无法进入桌面。
- **修复**（`apps/launcher` `lock_try_unlock`）：移除「PIN 满 4 位自动失败」，
  改为每输入一位做**哈希匹配即解锁**的增量校验（支持任意长度 PIN/密码），
  仅在按「确定」键、图案松手或输入满 16 位时才判失败。实测 6 位密码
  654321 正常解锁，错误输入不再被中途清空。
- **锁屏首帧提速**：背景由「桌面壁纸 + 双重压暗」（~150 万次混合/查表）
  改为逐行预计算的垂直渐变直写，OOBE 完成后锁屏更快出现，减少卡顿观感。
- **转场停滞压缩**：宿主机高负载 / TCG 卡顿下单帧可能耗时数百毫秒，启动/返回
  转场看起来"卡死"；主循环检测单帧 >250ms 时把转场时钟提前相应时间，使转场在
  1-2 帧内收敛到完成（应用立即出现），可感知卡顿上界压到约 250ms。

### 新增：系统锁屏模块（PIN / 图案 / 密码 / 滑动 + 远程锁定 + 胁迫 PIN）（2025-08-26 轮）

- **实现此前缺失的锁屏模块**（`apps/launcher`）：OOBE 早已保存安全配置
  （`security_type` / `security_hash` / `duress_hash` / `duress_action`，
  见 OOBE `save_security`）但没有任何模块消费它 —— 现在 launcher 启动时
  `load_security()` 读取并上锁：
  - **四种解锁方式**：无安全 = 上滑解锁；PIN = 3x4 数字键盘（输满 4 位自动
    校验）；图案 = 3x3 点阵（连线 ≥4 点松手校验）；密码 = 数字键盘 + 确定键。
    校验与 OOBE 一致的 FNV-1a 32 位哈希（`/data/oobe/security_hash`）。
  - **快捷面板「锁屏」行**：状态栏下拉（上滑手势）打开快捷面板，点击
    「锁屏」行立即上锁（替换原先的假通知占位行）。
  - **远程锁定**：主循环每秒轮询 `/data/oobe/locked`，云服务下发 lock 命令
    （cloud 应用写 1）后 1 秒内上锁。
  - **胁迫 PIN**：输入 duress_hash（默认 9999）触发 `duress_action`：
    fake_system（伪解锁提示「系统繁忙」）/ wipe（清除配置数据）/ alert（提示已上报）。
  - **锁屏界面**：压暗壁纸 + 5 倍放大 8x8 大时钟 + 日期（civil_from_days）+
    锁图标 + 解锁区；密码错误显示红字提示（3 秒自动消失）。
  - 锁屏通过手势完全旁路桌面逻辑（`handle_lock_input`），解锁后写
    `/data/oobe/locked=0` 并重绘桌面。
- 回归测试 `tools/apps_regression.py` 同步适配：OOBE 完成后检测
  `[lock] screen locked at boot`，用默认 PIN 1234 解锁后再进行应用测试。

### 修复：点击应用图标动画卡顿 / 疑似冻结（转场渲染提速 3-4 倍）（2025-08-26 轮）

- **启动/返回转场启用 2×2 块采样**（`apps/launcher` `ZOOM_BLOCK` 1→2）：
  此前注释说明块采样已就绪但等调度器 nanosleep 修复；该修复早已合入
  （`sched_cfs::tick` 到期唤醒 sleeper），现正式启用。`launch_blit` /
  `zoom_blit` 每帧像素操作从约 110 万次降至约 30 万次。
- **压暗与白闪改用预计算 LUT**：`darken_px` 用 256 项静态 LUT（每通道一次
  查表替代乘除），白闪每帧重建 256 项 LUT 后逐像素查表，删除逐像素
  `blend_white` 函数调用。实测：启动转场 9 帧/340ms → 18 帧/309ms（帧率翻倍），
  返回转场 6 帧/313ms → 18 帧/295ms（帧率 3 倍）——点击图标后的动画不再
  表现为「整个系统卡死」。

### 修复：OOBE 与全部应用鼠标移动卡顿（overlay 光标快速路径）（2025-08-26 轮）

- **OOBE**：删除每次 ABS_Y 移动的串口日志（`log_input("move", ...)`，每帧
  阻塞串口写入是光标严重不跟手的最大元凶）；鼠标移动不再整屏
  `render()+flip()`（TTF 重新栅格化），改为与 launcher 相同的 overlay 快速
  路径（`cursor_overlay`：11x11 区域恢复→备份→重绘，直接写显存 FB）。
  实测 40 帧鼠标扫掠 0 串口字节（此前每帧 ~40 字节）。
- **13 个 app_kit 应用**（phone/sms/contacts/browser/clock/calendar/camera/
  gallery/music/mail/map/weather + cloud）：`shared/app_kit.rs` 新增
  `cursor_overlay_redraw()` / `cursor_overlay_move()`；各应用 render 不再画
  光标进 back buffer，主循环 `ev.moved → dirty=true` 改为
  `cursor_overlay_move()`（只更新光标小区域），整屏重绘后调
  `cursor_overlay_redraw()`。
- **settings/files/calculator/store/stub**：新增共享模块
  `shared/cursor_overlay.rs`（参数化 FB 指针/stride/宽高/光标坐标），各应用
  以 `#[path]` 引入，`if moved { render(); flip(); }` 全部改为 overlay 移动。
- terminal 本就不渲染光标、不随移动重绘，无需改动。

### 新增：全栈报错处理（错误横幅 + Result/错误码 + 结构化云端错误）（2025-08-26 轮）

- **统一错误提示横幅**（新增 `shared/error_ui.rs`，接入 `shared/app_kit.rs`）：
  13 个 app_kit 应用经 `draw_sys_nav()` 自动在标题栏下方绘制暗红横幅
  （`show_error` / `error_banner_hit` 点按清除，4 秒自动消失）；calculator/
  files/settings/stub/terminal/store/oobe 等内联应用接入各自的
  `draw_error_banner`。OOBE 无墙钟，用帧计数计时（`err_set_ticks`/`err_tick`）。
- **输入设备缺失可见提示**：app_kit `poll_input` 在 `/dev/input/event0` 打不开
  时持续显示「输入设备不可用」横幅，恢复后自动清除（此前静默冻结）。
- **应用失败点补报错**：cloud 的恢复/ack/写盘失败不再静默（recv 错误与 EOF
  区分、恢复失败计数、凭证/锁定写失败提示）；launcher 移除应用写盘失败不再
  假成功、fork/execve 失败弹「应用启动失败」；settings 恢复失败不再假成功
  （写盘失败返回 -2 并显示失败状态）；oobe 全部配置写失败显示「配置保存失败」；
  store/terminal 子进程 execve 失败（退出码 127）给出明确提示；calculator
  加减乘溢出由静默回绕改为置 ERROR。
- **内核 syscall 错误路径修正**（`linux_compat.rs`）：修复 ~45 个未实现
  syscall 被误路由到 `sys_statx`（可能向垃圾用户地址写 256 字节、返回误导
  errno）→ 统一 `ENOSYS`；`getrlimit` 不再假装成功（写入默认
  `RLIM_INFINITY` 到用户缓冲区）；`sethostname`/`setsockopt`/`getsockopt`/
  `sigprocmask` 等不再假装成功；信号打断 syscall 返回 `-EINTR` 而非 0。
- **panic 与异常诊断补强**（`kunxcore/src/main.rs`）：panic handler 输出
  pid/tid/name/sp、栈回溯与返回地址扫描（与同步异常入口对齐）；异常向量表
  全部 FIQ/SError/SP0/AArch32 静默 `b .` 项改为打印 ESR/ELR/FAR 后停机。
- **bootloader/init/networkd**：`kunxboot` DTB fixup 由静默吞错改为返回 bool，
  失败打印原因并停机（bootargs 缺失内核将无控制台）；`kunxinit` 对
  networkd/OOBE/launcher 拉起失败与 OOBE 未完成给出明确告警；`networkd`
  管道创建/打开失败不再静默。
- **云端结构化错误**（`cloud/gateway/gateway.py`、`cloud/worker/src/worker.js`）：
  业务错误 `ERR:<code>` 首行保持兼容，同时映射真实 HTTP 状态码
  （bad_request→400、bad_auth→401、not_found→404、exists→409、
  internal→500）；内部错误脱敏（不再把异常文本回给设备，只写服务端日志）；
  Content-Length 畸形解析不再产生裸 HTML 500；Worker 备份整表替换改用
  D1 `batch` 原子提交；协议语义写入 `cloud/PROTOCOL.md`。
- **云端错误 detail 与设备端错误区分**：错误响应在 `ERR:<code>` 后追加第二行
  JSON detail（`code/message/retryable/http_status`，消息脱敏，旧设备只读
  首行完全兼容）；设备端（`apps/cloud`）新增 `resp_err_hint`，注册/备份/
  恢复/轮询失败时按错误码区分提示——bad_auth 显示「认证失败（账号或令牌
  无效）」、internal 显示「服务器内部错误」，不再一律折叠为「服务器不可达」。
- **剩余静默失败点补报错**：13 个 app_kit 应用 `font_init` 返回值不再忽略
  （新增 `kit::font_init_checked`，字体缺失显示「字体加载失败」横幅并串口
  告警，此前全部文字静默消失）；`now_us` 检查 gettimeofday 返回值（失败返回
  0，clock 显示 `--:--:--`、calendar 不高亮错误日期，此前显示栈垃圾）；
  map 缩放/gallery 翻页到达边界、browser 地址栏已满均给出横幅提示（此前
  点击/按键无任何反馈）；cloud `parse_server` 区分「服务器地址无效」与
  「域名解析失败」（此前 DNS 失败一律误报地址无效）。
- **本地应用剩余失败点补报错**：launcher `save_number`（WiFi/亮度）与启动
  门控 `gate_write` 检查写盘结果（失败弹「设置保存失败」/串口告警，此前假
  成功提示仍弹出），fork 失败时 `cancel_press()` 恢复图标按压态（此前图标
  永久卡在缩小态）；files 目录/文件打开失败显示「无法打开目录/文件」（此前
  空列表/无反应）；store fb 初始化失败打印诊断（此前静默退出）、软件目录
  打不开显示「软件目录加载失败」（此前假"0 packages"）；oobe `mkdir` 检查
  返回值（EEXIST 视为正常，真实失败告警）、账户表单校验（用户名非空/密码
  4-16 位（与锁屏 PIN 设计一致）/两次一致，不满足显示横幅并阻止下一步）；
  calculator 数字输入溢出
  置 ERROR 并提示（此前静默丢弃按键）；terminal 未知内置命令屏幕横幅提示
  （此前仅串口打印）。
- **内核用户可触发 OOM 不再 panic**（`vm.rs`/`sched_cfs.rs`/`elf.rs`/
  `process.rs`/`linux_compat.rs`/`bionic_shim.rs`）：中间级页表分配失败
  `map_page_in`/`map_page` 返回 bool、`add_page` 系列传播失败（不记录不一致
  映射）；任务栈 `alloc_stack` 返回 Option，fork/clone/spawn 失败上抛
  （-ENOMEM/u32::MAX）；用户栈页 OOM 走既有回滚路径；缺页落地/COW
  break/mprotect 映射失败归还新页并返回 SIGSEGV/错误，杜绝"记录指向新页而
  PTE 仍是旧页"的不一致与缺页无限重入。此前用户 mmap/fork/clone/exec 在
  物理内存耗尽时可直接打挂内核。
- **errno 粒度补强**（`vfs.rs`）：`mkdir` 区分 -EEXIST/-ENOENT（此前一律
  -1=EPERM）；`socket.rs` listen/accept 返回 -EBADF/-EINVAL/-ENOMEM（此前
  一律 -1）；`linux_compat.rs` `umask` 按 POSIX 返回旧值并更新（此前恒返回
  常量、忽略参数）。
- **测试稳健性**（`tools/apps_regression.py`）：启动等待从 60s 放宽到 150s，
  避免宿主机高负载（开发机同时跑 IDE/浏览器）时 QEMU 慢启动误判失败。

### 新增：内核安全硬化（PAN / W^X / ASLR / per-app UID）（2025-08-26 轮）

- **PAN（Privileged Access Never）内存硬化**（`mmu.rs`）：启动时探测
  SCTLR_EL1.PAN（bit 22，ARMv8.1）并启用（QEMU cortex-a72/max 均支持；
  ARMv8.0 核自动跳过）。EL1 访问用户内存必须显式进入 uaccess 区
  （`uaccess_begin`/`uaccess_end`），所有用户指针读写点（copy_from/to_user、
  read_cstr、futex、信号帧、getdents64/statx/ioctl 等）已逐一包装，杜绝内核
  绕过 `user_ok` 校验直接越权读写用户数据；EL1 同步异常处理器增加 PAN 故障
  诊断（漏包装时直接指出）。
- **W^X 执行防护**：`mmap`/`mprotect` 拒绝"可写+可执行"与任何 `PROT_EXEC`
  运行时映射（`EPERM`），可执行内存仅来自 ELF 加载（只读共享池页），阻断
  RWX 堆喷射/代码注入路径；`mprotect` 由 no-op 存根升级为真实实现
  （COW/共享只读页授予写权限时先 break 出私有副本，防止写穿其它进程）。
- **ASLR 地址空间布局随机化**（`elf.rs`/`vm.rs`）：每次 exec 随机化用户栈区
  （16MB 熵窗口）与堆基址（4MB 窗口）；匿名/文件 mmap 游标增设上限
  `MMAP_CAP`，与随机化栈区之间保留 15MB 隔离带（栈溢出 → 干净 SIGSEGV 而非
  静默写穿 mmap 区）；信号帧边界校验改用进程实际随机化栈基址；
  fork 子进程经 `elf::fork_runtime_info` 继承 ASLR 运行时信息。
- **per-app UID 安全模型**（Android 式沙箱雏形）：PCB 增加 uid/gid，
  `getuid/geteuid/getgid/getegid` 返回真实值，新增 `setuid/setgid`（仅 root
  可切换）；`kill` 增加权限检查（仅 root 或目标父进程可发信号）；launcher
  在 execve 应用前 `setuid(10000)`（与 Android 应用 UID 基址一致），应用进程
  无法 SIGKILL launcher/init/系统服务或其它应用；shell `ps` 显示 UID，
  execve 日志打印 uid 便于验证。

### 新增：打断动画（上划取消应用启动）（2025-08-26 轮）

- **应用启动转场（放大+白闪）未播完时上划** → 应用立即**变成图标沿滑动方向飞出**，
  悬停约 1 秒后用**弹簧飞回桌面原位**（iOS 取消启动风格）：滑动越快飞得越远
  （距离 = 120px + 速度×200，封顶 540px；实测快划 250px/80ms→540px、
  中速 120px/150ms→266px、慢划 60px/180ms→185px）。
- **手势识别使用内核事件时间戳**：`Touch` 记录按下/抬起事件的 `tv_sec+tv_usec`，
  手势时长不再受「事件成批处理」影响（此前批处理使 dur≈1ms、速度恒封顶）。
- **与并行启动门控联动**：打断时向启动门控写 `'2'`，已 fork 的应用读到后
  在绘制前退出，launcher 回收该子进程再播放飞行动画；桌面网格/Dock 中该
  图标原位在飞行期间隐藏，由飞行动画绘制在当前位置。
- **修复启动门控值域不匹配**：`gate_write` 此前写裸字节 `0/1/2`，应用侧
  `wait_launch_gate` 却按 ASCII `b'1'/b'2'` 比较 → 应用永远等满 3s 超时、
  取消信号不生效。改为写 `b'0'+v`，门控放行/取消即时生效。
- **修复字形缓存命中路径的堆泄漏**：`rasterize_char` 命中缓存时每字逐字拷贝
  `Vec`，而 bump 分配器永不回收，反复重绘（水波/飞行动画/分钟刷新）会把
  launcher 堆撑爆（brk 页表满 → Vec panic）。命中路径改为**借用**缓存槽位图
  （`Glyph.data: &'static [u8]`），渲染零分配；超大字形用静态备用缓冲兜底。
- **移除 COW 缺页逐页串口日志**（内核 `process.rs`）：每次 COW break 都做
  阻塞串口输出，启动动画期间每帧写显存触发数百次缺页 → 动画被拖慢数秒且
  阻塞 UART RX 导致注入触摸帧丢失；总量由 `COW_BREAKS` 汇总、引导结束统一打印。
- **修复 Makefile 共享源码依赖缺失**：`shared/ttf_text.rs`、`font_render.rs`、
  `nav.rs` 未列入各应用构建依赖，改动后 `make` 不触发重编（曾导致门控/字体
  修复未编进应用）；新增 `SHARED_SRC` 并挂到 launcher / settings / 全部应用规则。

### 新增：全面屏手势 + 三大虚拟按键系统导航（2025-08-26 轮）

- **新增 `shared/nav.rs` 系统导航共享模块**：导航模式（手势/三键）持久化在
  `/data/oobe/nav_mode`（"gesture"/"buttons"，缺省手势）；提供三键导航栏
  （返回/桌面/最近任务）与手势指示条绘制（`NavCanvas` 画布抽象，供各进程
  以单像素写入复用）、导航命中测试与全面屏手势分类：
  - 底部上滑 = 桌面；上滑悬停（≥450ms）= 最近任务；左右边缘内滑 = 返回
  - 手势/按钮模式与窗口布局解耦，设置变更即时生效
- **launcher 桌面**：Dock 上移至导航栏之上；桌面底部绘制导航栏（三键或
  手势药丸）；新增**最近任务面板**（最近应用 + 可见应用卡片网格，点击卡片
  启动、点空白关闭）；系统导航动作接入：桌面=回首页、最近任务=开关面板、
  返回=提示已在桌面；应用退出码约定 0=普通返回 / 1=回桌面首页 / 2=打开
  最近任务（`spawn_app` 按退出码恢复桌面状态）；鼠标点击导航栏同样生效。
- **全部 21 个应用接入系统导航**：13 个 `app_kit` 应用（browser/calendar/
  camera/clock/cloud/contacts/gallery/mail/map/music/phone/sms/weather）
  在 `render()` 末尾绘制导航栏，`poll_input` 增加手势追踪与 `PollResult.nav`
  动作，收到导航动作即按退出码退出；calculator/files/store/terminal/stub/
  settings 各自独立接入（`sys_nav` 模块 + 触摸手势追踪）。files 的返回
  实现智能回退（查看器 → 上级目录 → 退出应用）；terminal 全屏文本区不受
  导航栏遮挡；calculator 按键区上移避开导航栏。
- **OOBE 主题步骤增加「导航方式」选择**：全面屏手势 / 三键导航两个胶囊，
  选择结果实时预览在壁纸预览区底部；完成时写入 `/data/oobe/nav_mode`。
- **设置应用增加「导航方式」行**：显示当前模式（手势/三键），点击即切换
  并持久化；设置页底部绘制真实导航栏，返回/桌面/最近任务按钮可直接使用。

### 修复：TTF 字形垂直偏移 / 协议复选框不可点 / 鼠标卡顿 / launcher 启动崩溃（2025-08-26 轮）

- **修复 TTF 字形整体下移 17px**（10 处拷贝：OOBE、launcher、settings、files、
  calculator、terminal、stub、store、`shared/app_kit.rs`、内核 `ui_framework.rs`）：
  `ttf_glyph`/`draw_glyph_ttf` 之前按 `gy = y + ascent + g.ymin` 定位字形，
  但 fontdue 的 `Metrics.ymin` 是位图**底边**相对基线的偏移，叠加 `ascent`
  后每个字形都下移约 17px —— 截图对比确认：矩形控件（复选框/按钮）像素级
  精确，全部文本却整体偏低 17px。改为 `gy = y`（位图顶行对齐调用方 y，
  与旧点阵字体的「顶部对齐」语义一致），全部界面文字回到设计坐标。
- **修复 OOBE 协议步骤「我已阅读并同意」复选框钩不上**：复选框实际画在
  `(40, 228..240)`，而 `tap_agreement` 的命中区却是 `y∈[210,222)`，两者
  完全不重叠 → 点击复选框无反应。命中区改为覆盖复选框与标签文字
  `x∈[40,300) && y∈[224,248)`，与渲染位置一致。
- **修复鼠标不灵敏/指针拖影**：`shared/font_render.rs` 增加字形缓存
  （256 槽环，key = 码点+字号）。此前每次鼠标移动都会触发整屏重绘，而
  `rasterize_char`/`advance` 每帧对每个字形重新 fontdue 栅格化（无缓存），
  QEMU 模拟下帧率骤降、光标严重不跟手；命中缓存后仅做 memcpy。
- **修复 launcher 启动即 EL0 fault**（内核 `kunxcore/src/bionic_shim.rs` +
  `process.rs`）：每进程堆断点 `BRK_CUR[pid]` 在进程回收（`reap`/
  `reap_forced`）时未复位。OOBE（pid 3）加载字体后堆末在未对齐地址
  `0x39a71b0`，退出后 launcher 复用同一 pid，`brk_init` 因 `BRK_CUR[3] != 0`
  沿用旧断点 → 首个 `Vec` 分配返回的指针落在内核 `brk_set` 仅从页对齐处
  开始映射的区间之下，首次写入即触发 Data Abort。回收时复位断点/mmap 游标，
  launcher 可正常进入桌面并启动应用。

### 云服务：占位功能落地为真实端到端云服务（2025-08-25 轮）

- **服务端（生产）`cloud/worker/`**：Cloudflare Workers + D1 单文件 Worker
  （409 行，`wrangler.toml` + `schema.sql` + 内嵌管理后台 HTML）：设备注册 /
  数据备份整快照替换 / 恢复 / 心跳+命令轮询（find-lock-wipe）/ ack / 管理 API
  （设备列表、下发命令、查看备份）；令牌 FNV-1a 64 hex 存储，不落明文；
  42 项功能测试（内存 D1 shim）全过，与网关输出逐项比对一致。
- **明文 HTTP 网关 `cloud/gateway/gateway.py`**（Python 标准库，零依赖）：
  OS 客户机内核 TCP 栈无 TLS，经网关中转；生产模式反代 Worker（HTTPS），
  `--local` 模式 JSON 文件存储实现同契约，供 QEMU 内端到端联调；
  内嵌管理后台单页（设备列表 / 查找 / 远程锁定 / 远程擦除 / 查看备份）。
- **API 契约 `cloud/PROTOCOL.md`**：三端（客户机/网关/Worker）统一的行式
  明文协议：`OK/ERR` 响应、`F|<b64 path>|<b64 data>` 文件行、`C|id|type|payload`
  命令行；管理 API JSON。
- **客户机 `apps/cloud`「云服务」应用**（EL0，裸 syscall、no_std）：
  读取 `/data/oobe/cloud_*` 配置；经内核 TCP 栈实现 HTTP/1.0 客户端
  （socket/connect/sendto/recvfrom 系统调用 + 自研 base64/urlencode/FNV-1a）；
  注册设备、上传 /data/oobe 快照、云端恢复、轮询并执行命令
  （find→上报 IP、lock→锁定界面、wipe→擦除数据回首次启动）；GUI 状态界面。
- **共享框架 `shared/app_kit.rs`**：新增 socket 系统调用封装
  （`sock_open/connect/send/recv/close`、IPv4 解析）供后续应用复用；
  修复 `poll_tty_esc` 将 syscall 负错误码按无符号处理导致的越界 panic。
- **OOBE**：模块表扩至 18 个（新增「云服务」）；启用云服务时写入真实
  `用户名:<FNV-1a64 token>` 与默认服务器 `10.0.2.2:8080`（原为占位 `user:token`）。
- **launcher**：应用表扩至 18 个，第 1 页出现「云服务」入口。
- **内核**：`tcp_connect` 收到 RST（如端口未监听）立即失败返回，不再干等
  30s 超时（POSIX ECONNREFUSED 语义）。
- **端到端实测**：QEMU 内驱动 OOBE→桌面→云服务应用→注册→备份→
  admin 下发 lock/wipe→设备执行并确认，**全部 PASS**（对生产环境
  Cloudflare Worker+D1 实测）。
- **生产部署（2025-08-26）**：Worker 绑定自定义域名
  `kunxoscloud.kuankuantj.com`，明文 HTTP（80 端口）直达 Worker，客户机
  无需 VPS/网关；D1 建表、`ADMIN_KEY` secret（与 var 同名冲突已规避）、
  管理后台全部可用。
- **内核 DNS 系统调用（403）**：EL0 可解析域名（`net_stack::dns_resolve`），
  客户机服务器地址支持域名直填。
- **协议 bug 修复清单**：backup 头行 urlencode 解码（网关+Worker 同步）、
  请求体计数行换行、socket 关闭用 `SYS_SHUTDOWN`（防误关 stdout）、
  FNV-1a 64 素数常量、backup 文件计数两遍读一致（空文件不发 F 行）、
  网关反代加 UA 规避 Cloudflare bot 拦截（error 1010）。
- **联调顺带修复用户 WIP**：`shared/nav.rs` mutable static unsafe、
  launcher 缺失 `COLOR_DIM` 常量；内核偶发 /init PANIC 经干净重建后稳定
  （4 次启动 + 多轮 E2E 零崩溃）。
- **E2E 脚本 `tools/cloud_e2e.py`**：一键验证 注册→备份→服务器侧校验→
  admin 下发 lock/wipe→设备执行。
- **部署文档 `cloud/README.md`**：Workers+D1 部署、VPS 网关、cloudflared
  管理后台隧道、QEMU 联调步骤。

### 应用补全：全部 17 个桌面应用真实实现 + 全量回归实测（2025-08-25 轮）

- **共享应用框架 `shared/app_kit.rs`**：syscall 封装 / framebuffer 双缓冲 /
  绘制原语（像素/矩形/线/圆）/ ASCII 8x8 + CJK 16x16 字体 / 鼠标触摸键盘输入 /
  光标 / 标题栏返回键 / 墙钟时间与公历换算（Hinnant 算法），12 个应用经
  `#[path]` 共用，修复一处即可全量生效。
- **新建 7 个应用**（此前无二进制，点击会 execve 失败）：
  - `calendar` 日历：月视图网格、今天高亮、上月/下月翻页；
  - `camera` 相机：取景器 + 三分构图线 + REC + 快门闪光与缩略图；
  - `gallery` 图库：程序生成渐变照片网格 + 全屏查看 + 上一张/下一张；
  - `music` 音乐：专辑封面 + 进度条 + 播放/暂停/上一首/下一首 + 播放列表；
  - `mail` 邮件：收件箱列表 + 正文折行查看；
  - `map` 地图：街区/道路/河流网格 + 定位标记 + 缩放 + 罗盘；
  - `weather` 天气：当前天气卡 + 晴/云/雨图标 + 未来 5 天预报。
- **5 个占位 stub 升级为真实应用**：`phone` 拨号键盘（触屏+键盘输入、呼叫/挂断）、
  `sms` 会话列表 + 气泡对话、`contacts` 联系人列表 + 详情 + 呼叫按钮、
  `browser` 地址栏（virtio-keyboard 键入、退格、回车/前往）+ 书签 + 演示页面、
  `clock` 数字钟 + 模拟表盘（时针/分针/秒针，每秒刷新）。
- **全部 17 个应用默认可见**：OOBE 模块默认勾选从 9 个改为全部（含日历/相机/
  图库/音乐/邮件/地图/天气/终端），launcher 3x4 网格分两页 + Dock 展示。
- **既有应用 bug 修复**：
  - `terminal`：补 ESC 退出（此前打开后无法返回桌面）；`run_external` 路径缓冲
    溢出（`/bin/` + 27 字符以上会越界写 `path[32]`）改为截断到 26 字符；
  - `calculator`：除零之外补 `i64::MIN / -1` 溢出护栏（此前会 panic）；
  - `files`（本轮并行新写的真实应用）：`render_list` 的「共 N 项」计数器用 8 字节
    缓冲但写入 8+位数（根目录 20+ 项即越界 panic）；`poll_input` 非阻塞读返回
    `-EAGAIN`（转 usize 巨值）未按 isize 判负导致死循环、点击永不返回；
  - `store`：可用包索引路径 `/var/lib/pkg/available` 从未生成（目录显示 0 个
    软件包），Makefile 在 rootfs 打包时由 Packages 同步生成。
- **QEMU 全量回归**（触摸注入 + virtio-keyboard，17/17 通过）：逐个打开全部
  17 个应用并返回桌面；另抽查 phone 拨号/呼叫/挂断、calendar 翻月、
  music 播放/暂停、browser 书签页面，均正常；每应用截图存档
  （`tools/apps_regression.py`）。
- 说明：本轮验证期间另一并行任务正在重写内核 SMP 调度器，其早期版本在首次
  fork 处死锁；应用回归先以旧内核（拼接新 ramdisk）完成，调度器修复后已在
  当前内核复测通过。

### 五项新功能：符号链接 / virtio-rng / AC97 音频 / 文件管理器 / SMP 调度框架（2025-08-25 轮）

#### 符号链接（SFS + VFS + 系统调用 + shell）

- **SFS 新增 `T_SYM` inode 类型（6）**：目标路径作为文件数据存储，无盘格式
  变化；`fs_symlink`/`fs_readlink`，`fs_rename` 支持移动链接，`fs_unlink` 天然可删。
- **VFS 路径解析支持符号链接跟随**：`walk_from` 逐分量行走，绝对目标重新
  走挂载点、相对目标（含 `..`）从当前目录继续，8 跳环路上限；
  `resolve_parent` 重构为「目录部分 resolve + 末段不跟随」（可在链接指向的
  目录下创建/删除文件）；新增 `lstat_path`（不跟随末段）与 `symlink`/`readlink` API。
- **系统调用**：`symlinkat`（36）实现并移出 ENOSYS 组；`readlinkat`（78）从
  EINVAL 桩改为真实实现（截断语义）；`s_ifmt`/`dt_of`/statx 补 S_IFLNK/DT_LNK。
- **shell**：新增 `ln -s` 命令；`ls -l` 显示 `L` 类型与 `-> 目标`；`stat` 显示
  symlink 类型。
- **内核启动自检 24 项全过**：绝对/相对/子目录（`../`）目标、open/read 穿透、
  悬空解析失败、环路 hop 上限、链接指向目录下建文件、mv/unlink、lstat 类型。

#### virtio-rng → /dev/urandom

- 新驱动 `virtio_rng.rs`（DeviceID=4）：单 virtqueue「设备写入」描述符 +
  轮询 used ring，legacy/modern 兼容；注册字符设备主设备号 10，
  `/dev/urandom` 与 `/dev/random` 同源；`sys_getrandom` 优先用硬件熵源
  （无 RNG 回退伪随机）。
- QEMU 各运行目标加 `-device virtio-rng-device`；实测 `[rng] virtio-rng ready`。

#### AC97 音频（PCI ECAM + AC97 驱动 + 启动提示音）

- **极简 PCI ECAM 层 `pci.rs`**：从设备树解析 `pci-host-ecam-generic` 的 ECAM
  基址（QEMU 11.x = 0x4010_0000_00），配置空间 MMIO 读写、vendor/device 扫描、
  I/O BAR 分配（gpex 无固件，驱动自写 BAR）。
- **AC97 驱动 `ac97.rs`**（Intel 82801AA，QEMU "AC97"）：混音器复位/取消静音/
  EACS=0 固定 48kHz 立体声 16 位；NABM PCM 输出经 BD 链表 DMA（BD 格式按
  QEMU 源码语义：低 16 位长度=16 位采样单元、bit31 IOC、bit30 BUP）；
  正弦波合成（Bhaskara 近似正弦表 + 5ms 淡入淡出）。
- **启动提示音**（120ms / 880Hz）在启动时自动播放，顺带验证 DMA 播放路径。
- **端到端验证**：`-audiodev wav` 落盘，WAV 记录 0.242s @ 866Hz 正弦波
  （目标 880Hz，误差来自整数步进截断）。
- 内核恒等映射补 40 位物理地址支持：TCR.PS=40 位 + L1[256] 1GB DEVICE 块。

#### 文件管理器应用（apps/files，替换 stub）

- 全新 EL0 应用 `apps/files`：getdents64 列目录 + newfstatat 取大小、
  按名排序、点击目录进入/`..` 返回/文件打开文本查看器（翻页滚动）、
  标题栏返回键 + ESC 逐级退出；支持触摸与鼠标输入；CJK 中文界面
  （`shared/cjk_font_files.rs` 290 字子集）。
- Makefile 接入：独立构建目标，`/bin/files` 由 stub 占位替换为真应用。
- **GUI 端到端验证**：驱动 OOBE 完成 → launcher 桌面 → 点击 files 图标 →
  `[files] starting` → 渲染 540x960 文件列表（截图像素校验标题栏/返回按钮/
  行/背景色全部正确）。

#### SMP 多核调度框架

- **调度器 SMP 化**（`sched_cfs.rs`）：`CURRENT` 每核独立、全局就绪队列 +
  每核 idle 任务（槽位=CPU 号）、调度自旋锁（原子 CAS）「跨切换交接」：
  切换前获取、被切到任务的恢复点释放；`tick`/`yield`/`block`/`resched`/
  `wake*`/`reap`/`exit` 全部加锁；`start_secondary` 供 secondary 核进入。
- **首次运行解锁点**：`thread_trampoline`（spawn）与新 `el0_resume_entry`
  （fork/clone 子任务，先解锁再进 syscall_return_restore）——修复锁交接
  不变量被首次运行任务打破导致的 fork 后死锁（networkd/OOBE 无法 exec）。
- **每核 GIC/定时器**：`gic_init_secondary`（GICC per-CPU 偏移 0x20000）、
  secondary 核自建虚拟定时器 + 调度 tick；`_start`/`secondary_entry` 支持
  EL2→EL1 降级（secure=on 引导路径）。
- **QEMU 限制说明**：QEMU 11.1 `-kernel` 直载模式下 secondary 核被
  powered-off，需 PSCI CPU_ON 唤醒，而该模式无 PSCI 固件（secure=off 的
  smc 触发 UND，secure=on 的 EL3 桩不实现 PSCI）。`smp_init` 因此默认不
  调用 PSCI（避免 panic/挂死），多核调度代码在具备 PSCI 的环境（如真实
  EL3 固件）即可生效；`run-smp` 目标保留用于观察 cpus=N 与单核回退。
  单核默认路径完整回归（fork/exec/OOBE/launcher 全通，0 PANIC）。

### GUI 交互修复：OOBE 全步骤可点 + 账号输入框 + 应用占位页 + 应用内鼠标指针（2025-08-25 轮）

- **OOBE 选项可点击**：语言 / 模块勾选 / 地区时区 / 网络跳过 / 协议同意 /
  云服务与家长控制 / 锁屏方式与胁迫动作 / 主题与壁纸全部实现命中处理
  （此前只有「下一步/返回」响应点击，其余控件仅绘制不可交互）。
- **OOBE 账号输入框**：账号步骤改为三个真实输入框（用户名 / 密码掩码 /
  确认密码），点击聚焦（强调色边框 + 光标 `_`），支持 virtio-keyboard 键入
  与退格删除，回车推进焦点；`local_account` / `security_hash` 改用输入的
  用户名与密码哈希。
- **QWERTY 键位修复**：evdev 键码按 QWERTY 物理布局编号（KEY_Q=16、
  KEY_W=17…，非字母表顺序），键码 → 字符必须逐键显式映射（与终端应用
  `key_char` 同一张表）；修复后整排错位（Q→a、W→b…）消除，Shift 大写与
  符号（`shift-1`→`!` 等）实测正确：`qwertyuiop asdfghjkl zxcvbnm 1234567890`。
- **占位应用（stub）**：`/bin/phone|sms|contacts|files|browser|clock` 由
  通用 stub 应用提供（从 argv[0] 取应用名，全屏「敬请期待」+ 返回键），
  点击不再 execve 失败闪退（此前 6/9 可见应用无二进制）。
- **应用内鼠标指针**：calculator / settings / store / stub 均绘制跟随指针的
  光标并在移动时重绘（与 launcher/oobe 一致）；store 补充左上角返回键与
  ESC 退出（此前打开后无法返回桌面），点击改为按下+抬起完整手势判定。
- **ramdisk 扩容 512KB → 768KB**（内核 `RAMDISK_BLOCKS` 128→192 与
  `mkfs_sfs.py` 默认块数同步），容纳 6 个 stub 副本。
- 实测（QEMU 触摸 + virtio-keyboard 注入）：语言胶囊/模块勾选/输入框聚焦与
  键入均生效；9 个可见应用全部可打开可返回；各应用屏幕均可见鼠标指针。

### P1：信号 handler（sigaction / 投递 / sigreturn 蹦床）

- 每进程信号动作表（独立全局表，不膨胀 PCB 拷贝）：`rt_sigaction` 注册
  handler + mask；syscall 入口投递（`handle_pending`）。
- 投递机制：用户栈压 288 字节信号帧（TrapFrame 头 + signum），改写陷阱帧
  进入 handler（x0=signum，x30=蹦床地址）；handler 返回经蹦床
  `mov x8,#139; svc` → `rt_sigreturn` 恢复现场。
- 蹦床页：所有用户空间固定 VA 0x1FF00000 映射共享只读页，**标记永久页**
  （`allocator::mark_permanent`），杜绝共享引用计数归零后被释放复用。
- SIGKILL/SIGSTOP 直接终止；SIGINT/SIGTERM/SIGQUIT/SIGSEGV 默认终止；
  子进程退出时父进程注册了 SIGCHLD handler 才置位。
- 实测：`[signal-handler] SIGTERM caught!`，进程经 sigreturn 继续执行。

### P1：mmap 普通文件映射

- `sys_mmap` 文件分支：除 /dev/fb0 显存直映外，支持任意普通文件
  （MAP_PRIVATE 语义：读文件内容到私有页映射；每进程文件映射游标
  0x2E000000 向上）。
- 实测：mmap /etc/inittab 首字节 0x23（'#'）、写入私有页、munmap 全通过。

### P1：TCP 补全（拥塞控制 + 自适应 RTO + 指数退避）

- TcpSock 增加 cwnd/ssthresh/rto_us/last_send_us/retx_cnt。
- 新 ACK：RTT 采样（指数加权平滑）+ 拥塞窗口推进（慢启动 / 拥塞避免）。
- 超时重传：窗口收缩（ssthresh=cwnd/2, cwnd=1MSS）+ RTO 指数退避（上限 30s）；
  重传成功后退避复位。
- 实测：TCP 客户端经 slirp 连接宿主 echo 服务器，发送/接收回显通过。

### P1：tmpfs 内存文件系统

- 复用 ramdisk/mkfs 机制：`blkdev::ramdisk_create` + `fs_simple::mkfs` +
  `vfs::mount("/tmp")`（256KB）。
- 实测：写入/读回 /tmp 文件通过；`df` 显示 tmpfs 挂载。

### P1：HTTP 客户端（shell `http_get`）+ 既有 http_serve 服务端

- shell 新增 `http_get <ip> <port> <path>`：TCP 连接 → GET 请求 → 解析头部 →
  输出正文。
- 实测：客户机经 slirp 从宿主 HTTP 服务器抓取页面
  `[http_get] body (36 bytes): <h1>Hello from host HTTP server</h1>`。

### 关键 bug 修复：sys_nanosleep 误用墙钟时间（RTC 引入）

- RTC 改造时 `now_usec()` 改为墙钟（epoch µs ~1.78e15），但 sys_nanosleep
  复用该函数计算 deadline，而调度器 tick 用单调时钟比较 → deadline 永远
  达不到 → **所有用户态 sleep 永久阻塞**（networkd 卡死、shell 永不出现）。
- 修复：sys_nanosleep 改用 `timer::now_us()`（单调时钟）。

### P2：GDB 远程调试桩（gdbstub）

- 通过 UART 实现 GDB 远程串行协议（RSP）：`?`/`g`/`G`/`p`/`P`/`m`/`M`/`c`/`s`/`D`/
  `qSupported` 等命令；完整 aarch64 g 包（34 GPR + 32 向量 + fpsr/fpcr，788 字节）。
- shell `gdb` 命令进入调试循环（`gdb_break`：保存现场 → 屏蔽 IRQ → 服务命令）；
  断点期间批量排空 PL011 16 字节硬件 FIFO 到 4096 软件缓冲（防长报文丢字节），
  全程不触碰 DAIF（防定时器抢占切换任务污染会话）。
- 实测：宿主 `aarch64-linux-gnu-gdb` 连接后 `info registers` 显示真实寄存器
  （pc=0x400b2e80 在 sched_cfs::yield_now）、`x/4wx 0x40080000` 读出内核代码。

### P2：剪贴板服务

- 内核全局剪贴板（256 字节）+ 自定义系统调用 `SYS_CLIPBOARD_SET(401)` /
  `SYS_CLIPBOARD_GET(402)`（Linux 未占用号段）。
- 实测：`clipboard set=14 get=14 data=clipboard-data` PASS。

### P3：Raspberry Pi 4 后端补全（mailbox 帧缓冲 / GENET / EMMC2）

- **显示（真实实现）**：`plat_pi4.rs` 新增 VideoCore VI mailbox 属性通道驱动
  （channel 8：写/读寄存器轮询、bus 地址转换 0xC0000000、DC CVAC/IVAC 缓存
  一致性），向固件申请 HDMI 帧缓冲：1080p → 720p → 480p 逐级回退，返回真实
  物理地址 / 分辨率 / 32bpp。
- **网络**：GENET 千兆以太网（0xFD580000）返回真实基址 / 中断号
  （GIC_SPI 157 → 硬件 189）+ 预留 OUI MAC；**存储**：EMMC2 SD 卡（0xFE340000）
  返回真实基址 / 中断号（GIC_SPI 126 → 硬件 158）/ 块大小 512。
- 中断号取自 Linux `bcm2711.dtsi` 的 `GIC_SPI`（硬件号 = SPI + 32）。
- 内核侧 GENET / EMMC2 驱动接线与 gpu_simple 的 Pi4 适配留待后续；`make pi4`
  编译检查通过（内核 + HAL 全量，--features pi4）。
- 启动日志 `[hal] display=` 文案由 QEMU 专属 "ramfb" 改为平台无关 "display"。

### P3：磁盘加密（blkdev 透明 XOR 包装）

- **MBR 类型 0x8E 标记加密卷**：mkdisk 的 data 分区（sda3）改为加密类型。
- **`blkdev::wrap_encryption`**：把分区包装成加密块设备（新 major = 原 + 0x40，
  名字后缀 `e`），读 = 父设备读密文 → XOR 解密；写 = XOR 加密 → 写密文。
  上层文件系统（SFS/FAT32）完全无感知；内核启动时探测到 0x8E 自动包装。
- **演示密钥**（16 字节 "KunxOS-Enc-Key13"）+ 流密码：key 按「扇区号 +
  字节偏移」循环异或（同一扇区重复写使用相同流，可逆；不同扇区不同流）。
- 实测（QEMU + 宿主校验）：首次启动检测加密 → mkfs（密文写）→ 挂载 →
  写入 `/data/e.txt`；**重启后解密探测 SFS 成功、文件读回一致**；宿主查看
  disk.img：data 分区块 0 无 `SFS_` 明文魔数（密文乱码），XOR 解密后
  `0x5F534653` 魔数正确，文件内容仅解密可见。FAT32 分区（明文）与加密
  /data 共存正常。
- 说明：演示密钥非安全强度；真实系统应从 TPM / 用户口令派生密钥。

### 其它

- init 不再拉起已废弃的 displayd（其 30fps demo 依赖已禁用的动画定时器，
  会永久阻塞子进程回收）；launcher 直接操作 /dev/fb0 不受影响。

### P2：FAT32 可写（VFS 第二文件系统）

- **VFS 文件系统分派层 `fsops.rs`**：挂载时自动探测类型（SFS / FAT32），
  全部文件系统操作按 `dev → 类型` 分派，VFS 只依赖分派层；新增文件系统
  只需加一个分支。
- **内核 FAT32 读写模块 `fat32.rs`**：BPB 解析、FAT 表（FAT1/FAT2 同步）、
  簇链读写与分配/释放、目录项增删改查、8.3 短名转换（LFN 遍历时跳过、
  写入只生成短名）、mkdir/rm/rename/truncate/readdir 全支持。
- **ino 设计**：FAT32 无 inode，用静态「打开文件上下文表」把
  `(dev, 父目录簇, 目录项槽)` 映射为稳定 ino（根目录固定 1）。
- **性能**：每卷「下一空闲簇」游标（分配接近 O(n)）+ 每 entry 簇位置缓存
  （顺序读写从缓存续走），cp 1.6MB 文件由数十秒降至秒级。
- **mkdisk 修复**：boot FAT32 分区的 `fat_size` 此前只按文件内容计算
  （34 扇区 ≈ 4352 簇），而分区有 13 万簇——大簇号无 FAT 项导致内核
  分配失败；改为按整个分区计算。另补根目录簇 2 的 FAT 项置 EOC。
- **关键 bug 修复**（调试定位）：
  - fs_write 首次分配簇后内存 entry 的 cluster 未同步 → 第二次 write 重新
    分配新簇并覆盖目录项首簇（数据成孤儿）；
  - 缓存 `cache_off/cache_cluster` 错位 1 簇（终点 vs 所在簇）→ 顺序写
    覆盖旧尾簇、簇链少分配 400+ 簇、读提前 EOF；统一 `update_cache` 修复。
- **自动挂载**：启动时探测 MBR 各分区，FAT32 分区挂载到 `/mnt/fat32`
  （标准 `echo/ls/cat/cp/mkdir/rm` 直接可用）。
- 实测（QEMU virtio-blk + 宿主校验）：`echo > /mnt/fat32/hello.txt` 读写回、
  `mkdir` 子目录嵌套、`cp KERNEL.IMG`（1.6MB / 3232 簇）与源逐字节一致、
  `rm` 删除、`echo x >` 截断重写；宿主解析磁盘镜像：目录项、簇链、
  FAT1/FAT2 一致，KERNEL.IMG 跨簇链完整可读。
- 已知限制：shell `wc` 的 `read_all` 用 16384 固定缓冲（>16KB 文件只统计
  前 16KB，属既有工具限制，与 FAT32 无关）；FAT32 写入不生成 LFN。

### 中文界面 + CJK 字库（2025-08-25 轮）

- **共享 CJK 16x16 点阵字库**：`tools/gen_cjk_font.py` 从标准 HZK16 生成
  `shared/cjk_font_*.rs`（GB2312 一级字库全量给内核、按应用源码提取子集给各应用，
  规避 SFS 单文件 96KB 限制）。
- **内核 UI 支持 UTF-8 + 中文**：`ui_framework::draw_text` 解码 UTF-8，CJK 用
  16x16 点阵（ASCII 8x8 不变）；启动画面第二行改为「欢迎使用 KunxOS」。
- **launcher / settings / oobe / store 中文化**：应用名、标题、设置项、向导文案、
  按钮全部支持中文（launcher/oobe 按 `/data/oobe/lang` 双语切换，默认中文）。
- **OOBE 语言页默认简体中文**（`LANG=0` 即 zh_CN，模拟点击路径同样默认中文）。

### RTC 实时时钟（PL031）

- HAL `Platform` 新增 `rtc_base()`（QEMU virt = PL031 @ 0x09010000，Pi4 = None）。
- 内核 `rtc.rs`：启动时读取纪元秒，`gettimeofday`/`clock_gettime` 返回墙钟时间
  （纪元 + 单调时钟，无 RTC 退化为 uptime）；`date` 命令显示真实日历时间
  （civil_from_epoch，Howard Hinnant 算法，与 Python 标准库交叉验证）。
- 实测：`[rtc] PL031 epoch=1787645074 wall=2026-08-25 08:04:34`。

### 新系统调用：socketpair / truncate / statx

- **`socketpair(AF_UNIX, SOCK_STREAM)`**：ipc.rs 新增 PairSock（双环缓冲全双工），
  两个 fd 双向读写；`FdKind::Pair` 接入 read/write/close/dup。实测双向收发通过。
- **`truncate(path, 0)`**：按路径截断（O_WRONLY|O_CREAT|O_TRUNC，可创建文件）。
- **`statx`**：返回 statx 布局的类型/大小/mode 等基础字段（256 字节布局）。
- 修复 **VFS `resolve_parent` 循环不推进 `pos`** 的预先存在 bug：多级路径创建
  （`/tmp/x`、`/a/b/c`）此前全部失效（第二遍在子目录里重找同名分量）。

### procfs 只读虚拟文件系统

- 复用 devfs 字符设备机制（`ChrReadFn` 增加 offset 参数支持分次读取），
  `/proc` 下提供：`version` / `meminfo` / `uptime` / `cpuinfo` / `processes`。
- 实测：`cat /proc/version` → `KunxOS 0.1.0 (aarch64) rust-kernel`，
  `cat /proc/meminfo` → `MemTotal: 524288 kB`。

### 稳定性修复：EL0 PC=0 / 跳垃圾地址（预先存在的 FP 寄存器污染）

- **根因（新增证据）**：内核从不在异常/上下文切换中保存 FP/SIMD 寄存器
  （v0-v31 + fpsr/fpcr）。IRQ 处理中的 `memcpy`（NEON）与 `anim::frame_tick`
  的 f32 计算会静默破坏被中断任务的 v 寄存器；execve 新进程的陷阱帧 FP 区
  是内核栈残留（含 0.01f32 等浮点常量）→ 应用浮点计算错乱 → 跳垃圾地址。
  故障帧中 `x30=0x3c23d70a`（= 0.01f32）即铁证。
- **修复**：`irq_entry`/`lower_sync` 异常入口保存、出口恢复 FP 上下文
  （v0-v31+fpsr/fpcr，帧分别扩到 784/800 字节）；`execve` 清零陷阱帧 FP 区；
  禁用动画定时器 TIMER2（其 frame_tick 的 f32 是主要污染源之一，禁用后
  崩溃从 ~2 次/轮降到 ~0-1 次/轮，多次 0 崩溃）。
- 注意：DEBUG_NOTES 中「用户侧缓冲区溢出」结论被本次调查修正——指纹
  （0x3c23d70a=0.01f32、ESR 0x8a000000、x30 被浮点常量覆盖）指向内核 FP
  上下文缺失；仍有 ~10% 残余崩溃待后续定位。


### 信号投递补强（2025-08-18 凌晨轮）

- **`sys_kill` 唤醒目标进程**：kill 后立即唤醒阻塞中的目标（睡眠/等待中的
  进程下一次进入内核时 dispatch 检查信号位图并终止），SIGKILL 不再延迟到
  睡眠结束。
- **SIGKILL 投递端到端验证**：自 kill 测试应用（getpid → kill(self,9) →
  write）实测在 write 的 syscall 入口被终止（exit code=-1，无 "alive"
  输出）——此前只验证了置位，未验证投递。

### shell 增强（2025-08-18 凌晨轮）

- **`sysinfo` 增加 Uptime 行**：显示自启动以来的秒数（与 `uptime`/`date` 一致）。

### shell 新命令：od / tee（2025-08-18 凌晨轮）

- **`od <file> [-n <bytes>]`**：十六进制转储（每行 16 字节 + ASCII 侧栏），
  用于检查二进制文件（.pkg / ELF / 镜像）。
- **`tee <file>`**：把管道输入同时写入文件并透传到终端（`cmd | tee log`）。

### 信号投递补强（2025-08-18 凌晨轮）

- **`sys_kill` 唤醒目标进程**：kill 后立即唤醒阻塞中的目标（睡眠/等待中的
  进程下一次进入内核时 dispatch 检查信号位图并终止），SIGKILL 不再延迟到
  睡眠结束。
- **SIGKILL 投递端到端验证**：自 kill 测试应用（getpid → kill(self,9) →
  write）实测在 write 的 syscall 入口被终止（exit code=-1，无 "alive"
  输出）——此前只验证了置位，未验证投递。

### shell 增强（2025-08-18 凌晨轮）

- **`run` 显示子进程退出码**：`run <elf>` 等待子进程结束后输出
  `[run] pid N exited code=M`（含崩溃码，如 SIGSEGV = -11）；Sink 新增
  `emit_i32` 支持负数。

### 内核消息缓冲 + dmesg（2025-08-18 凌晨轮）

- **内核日志环形缓冲（16KB）**：`uart_send_char` 输出（含启动日志、panic
  前的内核消息）全部写入环形缓冲（IRQ 安全：DAIF 保存/恢复，可从任意
  上下文调用），覆盖最旧内容。
- **shell `dmesg [-n N]`**：回放内核消息缓冲；`-n N` 只显示末尾 N 行。
  供用户态查看启动日志（此前只能看串口实时输出）。

### shell 新命令：stat / true / false / basename / dirname（2025-08-18 凌晨轮）

- **`stat <path...>`**：显示文件类型（regular/directory/char-device/...）与
  字节大小（SFS 元数据，走 `vfs::stat_path`）。
- **`true` / `false`**：无操作命令（脚本条件占位）。
- **`basename <path>`**：输出路径最后一级分量；**`dirname <path>`**：输出
  目录部分（无分隔符时输出 "."）。

### shell 新命令：sha1sum（2025-08-18 凌晨轮）

- **`sha1sum [file...]`**：SHA-1（RFC 3174）校验和，支持管道输入与多文件
  参数；流式实现（分块 update，任意大小文件不截断）。已用标准测试向量
  与 Python hashlib 交叉验证（空串、含换行 abc/hello、busybox 二进制
  多块文件全部一致）。

### shell 新命令：date / which / uniq（2025-08-18 凌晨轮）

- **`date`**：显示自启动以来的运行时长（天:时:分:秒，基于内核单调时钟；
  内核无 RTC，不显示真实日历时间）。
- **`which <cmd...>`**：判断命令是内置命令还是 PATH（/bin、/usr/bin）
  中的可执行文件。
- **`uniq`**：去除管道输入中相邻的重复行（`ls | sort | uniq` 常用）；
  空输入直接返回。

### 信号投递与 kill/sleep 命令（2025-08-18 凌晨轮）

- **SIGKILL 投递**：`sys_kill` 此前只置位信号位图、从不生效；现在 `dispatch`
  入口检查当前进程待处理信号，SIGKILL（9）置位即终止进程（不可屏蔽）。
- **shell `kill <pid>`**：向进程发送 SIGKILL 并唤醒阻塞中的目标（下一次
  系统调用入口被终止）；拒绝自杀（kill shell 自身）与不存在的 pid。
- **shell `sleep <seconds>`**：基于内核单调时钟阻塞指定秒数
  （`sched_cfs::sleep_until`），可被定时器按剩余时间继续。

### 异常入口加固与 PC=0 纵深防御（2025-08-18 凌晨轮）

- **异常向量入口统一屏蔽 IRQ**：`sync_entry`（EL1 同步异常）与 `irq_entry`
  入口立即 `msr daifset, #2`。此前同步异常入口到 handler 的 daifset 之间
  存在窗口：定时器 IRQ 抢占会覆盖 `elr_el1/spsr_el1`，panic 诊断读到 IRQ
  残留值（故障 PC 报成抢占点、寄存器混入 IRQ 处理器残留），误导故障定位。
- **IRQ 返回 EL0 前校验 eret 目标（防「用户态 PC=0 / 执行内核代码」）**：
  irq epilogue 在弹出用户寄存器**之前**检查 `elr_el1`：返回目标为 0 或
  ≥0x40000000（内核地址）时，把 IRQ 帧转成 272 字节陷阱帧走 `el0_fault`
  干净终止进程，而不是 eret 到非法地址。校验在 pop 之前（x9/x10 随后被
  pop 恢复，不污染用户现场）；EL1 返回（M≠0）跳过校验，无误伤。实测
  25 轮 0 panic。
- **调试教训**：热路径（irq epilogue / syscall return / trampoline）新增的
  任何指令都会移动时序竞态窗口——此前为定位「memcpy(dst=2)」故障在热路径
  插入 last-eret 探针，把故障率从 ~7% 放大到 100% 每轮必现；移除探针后
  75+ 轮 0 PANIC。诊断探针只允许放在冷路径（fault/panic 分支）。
- **`virtio_input` 事件时间戳**：virtio 键盘事件此前 `tv_sec/tv_usec` 恒为
  0（`InputEvent::new` 置零），现用内核单调时钟填充，与 UART 触摸协议帧
  路径一致。
- **networkd 管道缓冲 256 → 1024 字节**：`poll_pipe` 读取命令后 `ftruncate`
  清空文件，>256B 的命令尾部此前直接丢失；1024B 覆盖全部现有命令。

### 新功能：文件操作内置命令与 SFS 重命名（2025-08-18 凌晨轮）

- **`touch <file...>`**：文件不存在时创建空文件（O_CREAT），已存在则空操作。
- **`cp <src> <dst>`**：复制文件；`dst` 为目录时复制为 `dst/basename(src)`；
  自复制（`cp a a`）明确拒绝，避免 O_TRUNC 先清空源再读导致数据全丢。
- **`mv <src> <dst>`**：重命名/移动文件，可跨目录（SFS 迁移同一 inode）；
  目标已存在、源为目录/不存在时明确报错。底层新增内核原语：
  - `fs_simple::fs_rename`：目录项「摘除 → 写入新目录」+ 失败回滚；
    拒绝 "." / ".."、目录与设备节点（避免破坏目录一致性）。
  - `vfs::rename(old, new)`：经 `resolve_parent` 解析两路径。
  - **`SYS_RENAMEAT`（38）实现**（原为 ENOSYS）：单根文件系统忽略
    dirfd，区分 ENOENT（源缺失）与 EEXIST（目标存在）。
- **`tail [-n N] [file]`**：输出末尾 N 行（默认 10）；无文件参数读
  stdin/管道；正确处理末尾无换行的文件（隐式换行），`-n 0` 输出为空。
- **`wc` 支持 `-l` / `-w` / `-c` 标志**（可组合，如 `-lw`；默认三者全出），
  与标准 wc 输出对齐；stdin 与多文件均支持，多文件末尾输出 total。
- **`ls` 支持普通文件参数**：`ls /tmp/f` 直接列出该文件（此前静默无输出），
  `-l` 显示类型/大小；路径不存在时输出 `ls: cannot access:`。
- `help` 文本与 Tab 补全表（BUILTINS）同步新增 touch/cp/mv/tail。

### 正确性 / 健壮性修复（2025-08-18 凌晨轮）

- **修复 fork 子进程用户栈越界写（根因级）**：`kunxinit::spawn_service`
  子分支原先把两个 96B 路径/名字缓冲与 argv 数组放在**栈局部**，编译器把
  清零/填充指令排在 `sp` 上方（`[sp+96..192]` 及 `[sp+192]`）；而 fork 子
  进程从 fork 返回时沿用父进程当时的用户栈指针（init 栈很浅，距
  `USER_STACK_TOP` 仅 ~176 字节余量），子进程会越过栈顶写进未映射的
  EL1-only 块 → 偶发「EL0 写权限故障」（此前误判为内核 TLB/页表问题），
  或覆盖栈顶 argv 数据引发后续 PC=0 / LR 损坏。修复：改用函数内
  `static mut` 缓冲（`P`/`NAME_BUF`/`ARGV`），彻底消除向 sp 上方的写。
- **fork 防御护栏**：`fork_parent` 拷贝子进程陷阱帧后校验
  `elr ∈ (0, USER_VA_TOP)`、`sp_el0 < USER_STACK_TOP`、`spsr & 0xF == 0`
  （EL0t），帧损坏时让本次 fork 失败而非调度一个「eret 到坏地址」的
  必死子进程。
- **UART 接收环形缓冲 256 → 4096 字节**：此前 shell 忙于执行命令时粘贴
  超过 256 字节的命令会静默丢弃尾部输入（表现为「收到半行命令后 shell
  永久等待换行」）。新增溢出丢弃计数 `UART_RX_DROPPED` + `uart_take_dropped`，
  shell 在每次读行前提示 `[shell] warning: N input byte(s) dropped`。
- **故障诊断增强**（`el0_fault_handler`）：新增用户栈回溯
  `[user-bt]`（沿 x29 链打印返回地址）与 `FAR_EL1` 打印，用于定位
  「LR 损坏 → ret 到 0」类故障的调用点。

### 残余 PC=0 故障机制最终确认（2025-08-18 第 17 轮）

- **系统调用 sp_el0 随机错位（systrace 实证）**：新增最近 24 次系统调用
  环形记录（nr/elr/sp_el0），故障运行显示用户 sp 在相邻 syscall 之间出现
  **随机错位**（init 故障中 -16 字节、oobe 故障中 +432 字节）—— 即陷阱帧
  `sp_el0` 槽被错误值污染，syscall 返回把错位后的 sp 恢复给用户，后续
  svc 持续维持错位，尾声最终读到被清零/错位的保存寄存器槽 → x30=0 → PC=0。
- 综合此前轮次结论（金丝雀排除栈溢出、fork 帧合法排除 fork 拷贝损坏、
  布局核对排除帧错位），残余 PC=0 确认为**陷阱帧 sp_el0 槽的罕见随机污染**
  （~5% 复现），由 `el0_fault_handler` 优雅终止，系统持续运行。
- **全链路回归验证**：shell 边界（cd 错误路径/`cd -`/重定向追加/跨目录 mv）
  全部通过；`http_serve` 经 QEMU hostfwd 端到端验证（宿主 curl 取回
  /var/www/index.html）。

### 残余 PC=0 故障机制定位（2025-08-18 第 16 轮）


- **机制确认（16 字节用户栈指针错位）**：对 init 最终打印故障的用户栈转储
  显示 —— k_main 的序言把 `x30=0x400010`（_start 的 bl 返回地址）保存到
  `[FB+0x60]`，但尾声从 `[FB+0x40]` 读取恢复寄存器（两者相差 16 字节），
  读到 0 → `ret` → PC=0。结合最后一次系统调用记录（write，sp=0x3effff40）
  与 fork 帧记录（fork 时 sp=0x3effff20 → k_main 帧基=0x3effff50），确认
  用户 sp 在 k_main 执行期间被**向下错位 16 字节**，后续所有 svc 均捕获并
  维持错位后的 sp，尾声因此读到错误的（清零的）保存槽。
- **新增诊断**：
  - `fork_parent` 记录每次 fork 子进程恢复现场（elr/sp/x30/spsr）—— 16+
    轮复现全部合法，排除 fork 陷阱帧拷贝损坏
  - `last_syscall` 增记用户 sp_el0；`el0_fault_handler` 增打
    `[last-syscall] nr/elr/sp` 与 `[user-stack]`（故障点附近用户栈转储）
- 结论：错位源是 syscall 返回/定时器抢占路径中的一次罕见 16 字节 sp_el0
  错位（每次 5-10% 概率），具体竞态点仍在追查；故障由 `el0_fault_handler`
  优雅终止，系统持续运行。

### 修复（2025-08-18 第 15 轮）


- **`ls` 目录项排序（真实用户可见 bug）**：此前 `ls` 按 SFS 目录项创建顺序
  输出（`/tmp` 显示 z,a,m），标准 ls 应按名称字典序。`cmd_ls` 现先收集
  最多 64 项到 .bss 缓冲，插入排序后输出（普通与 `-l` 模式一致）。
- **fork/clone 原子性修复**：`sched_cfs::insert`/`spawn` 此前无条件
  `daifclr` 恢复 IRQ，在 `fork_parent` 的 IRQ 屏蔽临界区中途重新使能中断 ——
  子任务会在 fork 尚未完成（父进程仍在其尾部）时即被调度。改为进入时读
  DAIF 的 I 位、退出时按调用方原状态恢复，保证 fork 全流程原子。

### 故障诊断与假设排除（2025-08-18 凌晨轮 · 残余 PC=0 追查）


- **任务内核栈金丝雀**：`alloc_stack` 用 0xA5 填充全部栈页，`el0_fault_handler`
  检查栈底 64 字节是否被破坏 —— **16+14 轮复现中 0 例破坏，明确排除内核栈
  溢出假设**（此前怀疑 fork 子进程内核栈越界覆写相邻任务栈顶）。
- **故障处理器增强**：打印 `FAR_EL1`（data abort 的故障访问地址）与故障任务
  内核栈顶 512 字节转储 —— 显示子进程栈顶下方残留 fork 时拷贝的父进程
  IRQ 帧（含用户 spsr/sp_el0 值），佐证残余 PC=0 与 fork/exec 首段执行、
  定时器抢占的寄存器恢复竞态相关。
- **页释放路径审计**：fork/exec/exit/reap 的 `free_page`/`free_user_pages`/
  `free_process_tables`/`reap_pid` 全路径复查，未发现双重释放或提前释放
  （create_process 的 spawn 失败路径仅泄漏页表页，已记录为 Low）。
- 结论：残余 PC=0（约 1/10-1/16 复现）为 fork 相关寄存器恢复竞态，由
  `el0_fault_handler` 优雅终止并重启，不挂死内核；诊断输出已就位待后续定位。

### 正确性 / 安全修复（2025-08-17 深夜轮）

- **bootloader 加固（审计二轮）**：
  - `dtb::walk_node` 增加递归深度限制（MAX_DEPTH=16）——防恶意/损坏 DTB
    构造深嵌套节点导致 bootloader 栈溢出
  - `fs_fat::fat_entry` 校验 FAT 表扇区边界（越界一律视为簇链结束）——防
    损坏簇号使 fat_sector 落到数据区/分区外，读到无关数据作为 FAT 项
- **图形/输入子系统补强（审计二轮）**：
  - `compositor::set_scale` 钳制 `scale ∈ [1, 1024]`（防 `visual_rect` 的
    `w*scale` 回绕产生巨大可视矩形）
  - `ui_framework` Image 控件：null/0 尺寸跳过 + 源尺寸钳制到目标矩形
    （防 `blit` 越界读任意内存）
  - `timer::reload`：freq 或周期为 0 时用最大间隔兜底（此前 tval=0 →
    定时器立即重触发 → IRQ 风暴饿死低优先级中断）
  - `virtio_input::drain`：坏 used id 钳制到队列范围再重投递（防 desc 槽位
    逐步耗尽导致输入队列枯竭）
  - `input::handle_frame`：坐标钳制到设备物理最大值（max_x/max_y）

- **用户指针页表漫游校验（防 EL0 DoS）**：`user_ok`/`copy_from_user`/`copy_to_user`/
  `peek_u32`/clone tid 写回全部增加「页已映射」验证（`vm::user_range_mapped`）。
  此前只验范围不验映射，用户传未映射地址会在 EL1 触发 data abort，同步异常
  处理器死循环挂死整个内核（任何用户进程可借此 DoS）
- **EL0 用户程序故障隔离**：`lower_sync` 检查 ESR 区分 SVC 与其它同步异常
  （data/instruction abort、PC 未对齐等）；非 SVC 走 `el0_fault_handler`
  终止当前进程（SIGSEGV 语义），由 init 回收重启 —— 用户程序崩溃不再
  拖垮整个内核（oobe 无帧缓冲时曾稳定触发内核挂死）
- **任务栈 128KB → 256KB + `create_child_process` 引用传参**：ELF 加载器在
  栈上持有 `ProcPageTable`（1536 条记录 = 24KB）且 fork/execve 调用链深，
  128KB 栈在 oobe（2MB BSS）execve 时溢出、写穿栈底破坏物理页位图，
  导致后续栈分配重叠、内核崩溃 —— 256KB 栈 + 避免页表按值传参彻底消除
- **`MAX_USER_PAGES` 512 → 1536**：launcher 含两个 2MB 帧缓冲（约 1017 页）
  + 1MB 栈（256 页），512 项记录打满导致「只建映射不记录」的泄漏与
  fork 父子地址空间不一致；记录表满现在**拒绝映射并回滚**（`add_page` 返回
  bool，全部调用方处理失败），而不是静默丢记录
- **ELF 加载失败路径统一清理**：`load()` 所有错误路径释放已分配页与页表
  （此前恶意/损坏 ELF 反复加载可耗尽物理内存）；`e_entry` 校验（入口必须
  落在用户地址空间）；`setup_stack` 失败回滚
- **`fs_simple` 加固**：inode 号边界校验（防 ino≥64 读写数据块区）、
  `fs_write` 检查 read/write_block 返回值（防 RMW 读失败清零整块）、
  `fs_create`/`fs_mkdir` 失败回滚（防 inode 泄漏与半成品目录）
- **`blkdev` checked 运算**：`block*8`、`start_lba+sector`、分区 `start+count`
  全部 checked（防 usize 回绕读写错误扇区）
- **`allocator::init` 钳制**：`kernel_pages` 截断到 `TOTAL_PAGES`（防链接
  符号异常时位图越界写）
- **`vm::add_page` 记录表满拒绝映射**：不再静默丢记录（泄漏 + fork 不一致 +
  回滚释放错页），全部调用方（fork/brk/mmap/ELF）处理失败并回滚
- **input 等待者多槽化**：`waiter` 单槽 → 4 槽数组（多任务阻塞读不再互相
  覆盖丢失唤醒）；非阻塞 `poller` 登记屏蔽 IRQ
- **`pkg install` 资源路径缓冲 96 → 128**（最长 99 字节路径不再截断丢 NUL）
  并检查 `write_file` 返回值
- **`write_stat` 失败传播**：stat 指针非法返回 `EFAULT`（此前静默成功）
- **`sys_getrandom` 增强**：熵混入 tid + 拷贝失败返回 `EFAULT`；
  `sys_recvmsg` 拷贝失败返回 `EFAULT`
- **`sys_close` 语义修正**：未登记的 fd 返回 `EBADF`（此前无条件成功）；
  补 `MsgQueue` 关闭分支；socket 句柄仅在确为 socket 时关闭
- **`sys_kill` 校验（M-3）**：信号号 ≥64 返回 `EINVAL`；目标进程不存在
  返回 `ESRCH`
- **displayd/networkd ftruncate 显式 length=0（M-20 修复补全）**：此前只传
  一个参数，x1 残留值导致内核按 `length!=0` 拒绝，命令管道从不被清空
  （命令重放 / 文件无限增长 / >256B 后新命令读不到）
- **displayd `SURF_META` 下标修正（M-21 修复补全）**：1 基 id 回退 1 再作
  数组下标（此前 w/h 写错槽位、第 8 个 surface 越界写坏 SURF_BUF）
- **合成器 IRQ 同步（H-12 残留）**：`create/destroy/resize_window` 全程屏蔽
  IRQ 且「先摘除窗口再释放表面」—— 杜绝定时器 IRQ 侧 `composite()` 读取
  已 `kfree` 的表面（use-after-free）
- **动画弹簧数值稳定性**：`Spring::step` 钳制 `mass`/`stiffness`（防除零与
  半隐式欧拉发散）并在积分出 NaN 时强制收敛 —— 防止动画永不结束持续
  驱动整屏渲染
- **bootloader mmc used-ring 轮询超时**：设备挂起时不再死循环
- **`dup`/`dup3` 目标槽位关闭 + 引用计数**：dup3 覆盖已占用槽位时按 close
  语义释放（引用 -1），File/Shm 复制时引用 +1，防对象提前释放

- **fork/clone 子进程恢复路径重构（C-6 残留）**：子进程/子线程首次调度直接进入
  `syscall_return_restore`（sp 指向其拷贝的陷阱帧、x0 已置 0），不再沿父调用链
  回卷 —— 消除「子进程把返回值写回父进程陷阱帧」的跨栈写，也不再有陈旧帧写
- **管道 / 消息队列引用计数（H-7 延伸）**：`refs` 计数（创建/复制 +1，close/exit
  -1，归零才清槽）—— fork 子进程退出不再清空父进程仍在使用的管道/队列
- **真实 `dup`/`dup2`/`dup3` 语义**：复制 fd 表项并共享底层对象（文件共享偏移，
  引用计数同步），替换原先「返回原 fd」的假实现
- **`pread64`/`pwrite64`/`lseek` 64 位偏移（M-4）**：不再 `as i32` 截断
- **per-pid brk / mmap 游标**：`BRK_CUR`/`MMAP_NEXT` 从全局单值改为按进程维护，
  进程 B 不再继承进程 A 的堆断点 / 匿名映射游标
- **execve 静态缓冲竞态修复**：`EXEC_PATH/ARGV/ENVP` 从 static 改为系统调用
  栈帧、ELF 读入缓冲从 static 改为按需堆分配 —— 消除定时器抢占下并发 execve
  覆写共享缓冲导致的加载污染
- **`sys_sysinfo` 真实内存统计 / `sys_uname` 名称修正**：内存按物理页分配器
  实时统计（此前硬编码 128MB）；`uname` 从 "NovaOS" 改为 "KunxOS"
- **virtio-blk used-ring 完成项 id 校验（H-A）**：每次请求使用轮转描述符组并
  校验 used 项 id —— 超时请求的迟到完成不再被误判为下一次请求成功（消除
  use-after-free 与读写错乱）
- **`open_file` 失败回滚（M-A）**：fd 表满时回滚已占用的打开文件表槽位，
  不再泄漏（32 槽耗尽 DoS）
- **unlink 拒绝删除仍打开的文件（H-B）**：防止 inode 号复用后旧 fd 与新文件
  形成逻辑别名
- **`pkg list` 走管道 Sink**：`pkg::list_to` 回调输出，`pkg list | cat` 下游
  可收到内容（此前直接 uart 输出绕过管道）
- **kunxpkg CLI 资源区边界 + 包名/资源名白名单（M-18/M-19 用户态侧）**：
  `parse` 逐条校验资源区长度、`valid_name` 拒绝 `/` 与 `..`（路径穿越）；
  `arg()` 改安全 UTF-8 校验
- **fdt.rs 字符串安全 UTF-8**：`read_cstr`/`string_at` 从 `from_utf8_unchecked`
  改为安全 `from_utf8`（任意字节 DTB 不再触发 UB）
- **FDT 头解析修复（virtio 设备检测回归）**：`size_dt_struct` 误读偏移 16
  （off_mem_rsvmap，典型值 ~48）导致结构块边界提前截断，virtio/net/blk 深层
  节点全部扫描不到 —— 改读偏移 36（size_dt_struct）。**此前 `/data` 持久化与
  真实网络收发从未生效**（networkd 的 tx/rx 为本地计数器，设备未被检测）
- **virtio-blk 驱动可用（/data 持久化真正工作）**：修复三处互相关联的缺陷——
  ① 寄存器布局按 QEMU virtio-mmio 实际偏移（legacy/modern 共享
  QueueSel@0x030/Status@0x070 等，此前误用 virtio-1.0 PCI legacy 偏移）；
  ② 环内 used ring 需按 QueueAlign（=PAGE_SIZE）对齐到独立页，环改分配 2 个
  连续页；③ avail 偏移此前误按「每请求 3 描述符总表长」计算，与设备按
  QUEUE_NUM 计算的布局不符。修复后 MBR 解析、sda1/2/3 分区注册、/data SFS
  挂载与持久化全部可用
- **virtio-blk 超时门控（H-A）**：请求超时后泄漏缓冲并置 `TIMEOUT_PENDING`，
  下次提交前先轮询消费陈旧完成项 —— 杜绝「把超时请求的迟到完成误判为本次
  成功」导致的 use-after-free 与读写错乱
- **TCP 序号回绕比较（RFC 1982）**：`seq_lt/gt/ge` 有符号差比较替代普通
  `<`/`>`（累计传输 ~4GB 后 32 位序号回绕，旧比较导致 rtx 提前清除、
  send 未等 ACK 即成功）
- **H-5 修复补全（重叠段补入队 + FIN 边界）**：对端重传整段而缓冲曾满时，
  段尾超过 `rcv_nxt` 的部分（未接收的新数据）补入队而不是当作纯重复丢弃
  （此前尾部数据永久丢失）；`data+FIN` 满缓冲场景下仅当 FIN 序号 == rcv_nxt
  才推进并 ACK，不再确认未入队数据
- **RST 后 recv 立即 EOF**：`T_CLOSED`（对端 RST）与 `peer_fin` 同等视为 EOF，
  不再干等 60s 超时
- **DNS label 长度上限**：>63 字节 label 直接跳过（此前 256..511 字节 label
  `as u8` 截断成畸形查询）
- **virtio-net used ring 缓存失效 + RX 初始 clean**：`drain_impl` 读 used ring
  前 `dcache_invalidate`（此前只 invalidate RX 数据缓冲，KVM/真机上 TX 永不
  回收、RX 永不收包）；`rx_ring_fill` 初始 desc/avail 写回缓存（防设备冷启动
  读到陈旧 desc addr=0）
- **mkfs 清零 inode 表块（L-A）**：新盘未分配的 inode 槽不再残留分配器
  随机内容（损坏盘/异常路径不会把垃圾当 T_DIR + 垃圾块号处理）
- **`fs_write` 超限报错（L-B）**：写入位置超过单文件上限（96KB）且一个字节
  未写入时返回 -1（此前静默返回 0，看起来「成功写了 0 字节」）
- **合成器 DAIF 保存/恢复（修复嵌套中断崩溃）**：`create/destroy/move/resize/
  set_z/set_alpha/set_scale_window` 的 IRQ 屏蔽从「无条件 daifclr」改为
  **保存原 DAIF 再恢复** —— `set_scale`/`move_window` 会从 IRQ 上下文
  （`anim::frame_tick` → `apply`）被调用，无条件 `daifclr` 会在中断处理器
  中途重新开中断 → 嵌套中断 → 内核跳转垃圾地址（ELR=0 / 0x4a / sp 进入
  用户栈区）随机崩溃
- **窗口销毁取消动画**：`destroy_window` 调用 `anim::cancel_window` ——
  窗口 id 复用后不再被残留旧动画驱动
- **compositor 全部 mutator 屏蔽 IRQ**：`move_window`/`set_z`/`set_alpha`/
  `set_scale` 与 IRQ 侧 `composite` 互斥（此前只有 create/destroy/resize 屏蔽）
- **input 读取路径 DAIF 保存/恢复**：`read_events`/`input_read` 从无条件
  `daifclr` 改为保存/恢复（嵌套屏蔽上下文不再提前开中断）
- **`visual_rect` u64 运算**：宽扁窗口 `w*scale` 不再 u32 溢出
- **ui_framework vbox/hbox 溢出钳制**：`item_h/gap` 超过 i32::MAX 不再回绕
- **panic 处理器防递归**：入口屏蔽 IRQ 并只打印首次故障（损坏 sp 下递归
  panic 不再无限刷屏）
- **fork/clone 恢复为「整体栈拷贝」设计**：子进程内核栈拷贝父栈活动区
  （含陷阱帧），拷贝区之下保留完整可用栈（~256KB）。修复并行重构
  「仅拷陷阱帧到栈顶」方案的偶发子进程 syscall 返回帧损坏（eret 到内核
  地址 → EL0 非法执行 → 进程被杀）；保留 `ctx.elr` 从帧读取（消除
  ELR=0 在飞窗口）与 `syscall_return_restore` 的 eret 目标校验
  （elr 必须落在用户区，否则终止进程，防故障→恢复→再故障循环）
- **fork/clone 陷阱帧放子栈顶部**：结合整体栈拷贝，子进程首次 eret 后内核
  sp = 子栈顶，后续 syscall 帧从栈顶向下分配 —— 与拷贝区（含 exec 的
  exec_path 等残留数据）隔离，进一步降低「残留污染 syscall 帧 elr → eret
  到 0」的偶发故障（配合 el0_fault 隔离，内核始终稳定）
- **★ syscall_return_restore 入口屏蔽 IRQ（消除 eret 到内核地址故障）**：
  syscall 体被 `daifclr` 放开抢占后，若定时器 IRQ 恰在「msr elr_el1 →
  eret」窗口抢占任务，switch_to 会把 elr_el1（=syscall 返回代码地址）存进
  任务上下文，恢复时 eret 跳回内核地址 → EL0 执行内核代码 → 故障。修复：
  恢复+eret 全程 `msr daifset`（eret 到 EL0 自动开中断），显著降低偶发
  用户态跳转内核地址的故障率（残余已被 el0_fault 隔离，内核始终 0 panic）
- **/data 持久化端到端验证**：echo 写入 → 重启 → cat 读取，文件跨重启保留
  （virtio-blk 修复后的完整闭环；此前 /data 从未真正工作）
- **shell 新命令验证**：`ps`/`uname`/`uptime`/`calc`（运算符优先级正确）/
  `ls`/`history`/`sysinfo` 等全部可用
- **★ 网络栈端到端验证（此前网络从未真实工作）**：virtio-net 驱动 + net_stack +
  socket 全链路可用 —— `ping 10.0.2.2` 收到 ICMP 应答；`nslookup example.com`
  真实 DNS 解析出 104.20.23.154；**`http_serve 8080` 经 QEMU slirp hostfwd 被
  宿主机 curl 访问，返回完整 HTML**（TCP listen/accept/read/write/close 全链路，
  含 T_LAST_ACK/序号回绕/H-5 修复的实际验证）
- **fdt `prop_str` 安全 UTF-8**：DTB 属性值不再 `from_utf8_unchecked`（任意
  字节输入为 UB；`read_cstr`/`string_at` 上一轮已修）
- **gic 越界防护**：`gic_set_priority`/`gic_set_target` 补 `irq < 1020` 断言
  （此前只有 enable/disable 有；irq≥1020 会写穿 GICD 寄存器窗）
- **shell normalize 溢出报错**：路径分量 >32 或超长分量放不下缓冲时返回空串
  （命令报错），不再静默截断成另一条路径去操作错误文件；`cd` 校验目标必须
  是目录（此前 `cd 普通文件` 会"成功"）
- **bootloader RAMDISK_MAX 注释修正**：0x80_0000 = 8MB（此前注释误写
  "128MB"）

### 正确性 / 安全修复（调度、内存、网络、文件系统）

- **★ 调度器上下文切换恢复点修复（消除 EL1 PC=0 崩溃）**：`sched_cfs::tick`/
  `yield_now`/`block_current`/`exit_current`/`sleep_until` 与 `timer::timer_handle_irq`
  全部 `#[inline(never)]`。此前 tick 被编译器内联进 `timer_handle_irq` 后，任务在
  tick 内 `do_switch` 的恢复点落在调用方中段，其后的 `pop x30; ret` 弹出错误栈槽位
  → 跳转垃圾地址（同步异常 ELR=0 / 用户态 PC=0）——表现为 fork/exit 后随机崩溃；
  多轮 QEMU 冒烟（内核直启与 bootloader 磁盘启动）均 0 崩溃
- **内核映射 W^X 强化（vm.rs）**：内核栈 / 数据 / BSS / 堆 / 页表 / ramdisk 等
  数据区映射全部加 `PXN`（EL1 禁止取指），低 1GB 设备区同理——消除「在内核
  数据区执行代码」的提权路径
- **`kfree` 边界校验（heap.rs）**：slab 释放校验指针归属（页内 + 对齐 + 非重复），
  大块释放校验页对齐与魔数——非法/重复释放不再破坏空闲链表
- **net_stack 加固**：ARP 协议字段校验（htype/ptype/hlen/plen）+ 拒绝学习
  0.0.0.0 与自身 + 缓存满时 LRU 淘汰；DNS 应答要求源端口=53 且查询未完成
  （防伪造应答）；IPv4 目的地址过滤（非本机/广播直接丢弃）；UDP 超 MTU
  载荷返回失败而非静默截断
- **socket 修复**：`bind` 端口占用检查（EADDRINUSE）；UDP 未绑定自动分配
  临时端口；重复/旧数据段回 ACK 帮助对端推进；**FIN 重传登记**（挥手阶段
  重传 FIN|ACK 而非旧数据段，M-10 部分修复）
- **virtio 驱动加固**：net_virtio RX 软件队列 32→64（与环深一致，突发不丢包）；
  `kmalloc` 返回值检查（失败不向地址 0 投递描述符）；队列 `QUEUE_NUM_MAX < 环深`
  时拒绝初始化（net_virtio 与 virtio_blk）
- **gpu fw_cfg DMA 等待超时**：设备异常时不再死循环
- **fdt 解析边界检查**：`struct_end`/`strings_size` 边界校验，损坏 DTB 不再越界读
- **shell 增强**：新增 `uname`/`clear`/`mkdir`/`rm` 内置命令；命令历史（↑/↓ 方向键
  浏览，16 条）；管道/重定向输出截断告警（>4096B 提示而非静默截断）
- **IPC 并发正确性（ipc.rs）**：管道/消息队列读写临界区全程屏蔽 IRQ（「检查→操作→
  唤醒」原子化，消除单核抢占下环形缓冲竞争与「登记后阻塞前」丢唤醒）；`wait_wake`
  改用 `wake_irq`；对象池满返回错误而非 panic（防 DoS）
- **input_stack 坐标溢出**：`to_screen` 定点缩放改用 i64 中间量（u16 全量程坐标
  不再 i32 回绕）
- **进程工作目录（M-2 部分）**：`chdir`/`getcwd` 真实实现（per-process cwd，
  fork 继承、exec 保留）；`openat`/`mkdirat`/`unlinkat`/`mknodat`/`faccessat`/
  `newfstatat` 相对路径按 cwd 解析
- **brk 收缩释放物理页（bionic_shim）**：`brk` 缩小断点区间时归还页并移除地址
  空间记录（此前只 unmap 不释放，反复 shrink/grow 泄漏页）
- **MAP_FIXED 语义补全（linux_compat）**：覆盖已有映射前先释放旧页
  （`unmap_user_range`）；分配中途失败回滚本次映射（不泄漏）
- **★ 文件系统 inode 容量修复**：`MAX_INODES` 64 → 128（内核 + mkfs_sfs.py 同步）。
  64 个 inode 在首次启动复制 16 个 OOBE 默认配置到 /data 后即耗尽，导致
  mkdir/写文件全部失败（inode 表 2→4 块，数据块仍充足）
- **rootfs 补 /tmp 目录**（Makefile），修复 `mkdir /tmp/...` 与临时文件场景
- **bootloader 跳转前缓存维护（jump.rs）**：`ic iallu` + 对已加载镜像区间
  `dc civac`（真机取指/读数据保证最新）；`load_all` 返回各镜像尺寸供维护
- **`sys_fcntl(F_DUPFD)` 真复制**：复用 `dup_fd_common`（此前返回原 fd 假语义）
- **物理页分配器按真实内存钳制**：`allocator::init` 按 DTB 的 `/memory` size
  限制可分配范围（QEMU 512MB，此前按 1GB 管理，超量分配会触碰未映射内存
  data abort）；`total_pages`/`free_page` 同步按限制校验；`sysinfo` 如实显示
  MemTotal
- **shell `sysinfo` 命令**：内存 / 任务 / 进程实时统计（MemTotal/MemUsed/Tasks）
- **接收端 IP/TCP/UDP 校验和验证**：`handle_ip` 校验 IP 头校验和与 TCP/UDP
  传输层校验和（含伪头，0 表示发送方未计算则跳过），损坏包直接丢弃，不再
  被当作有效数据解析/破坏连接状态
- **compositor Rect 溢出加固**：`right`/`bottom` 饱和加法（极端窗口位置/尺寸
  组合不再 i32 回绕）
- **★ SFS 位图容量钳制（fs_simple）**：单块位图最多跟踪 32768 数据块（128MB）；
  `total_blocks` 按位图容量钳制设备容量——修复 256MB /data 分区上 `df` 统计
  失真（把位图覆盖不到的块算入总量）与 mkfs/分配器计数不一致
- **★ TCP 服务器支持（listen/accept）**：socket 层新增 `T_LISTEN`/`T_SYN_RCVD`
  状态与待 accept 队列——服务器侧三次握手（SYN→SYN-ACK→ACK）、`listen()`/
  `accept()` 系统调用接线、监听套接字关闭时释放子连接；实测 QEMU hostfwd
  从宿主机完成回显（单连接 / 3000 字节大块 / 多连接均 PASS）
- **★ HTTP 服务器静态文件服务**：`http_serve <port> <root_dir>` 提供目录下
  文件（GET / → index.html、MIME 推断、404、拒绝 `..` 穿越）——实测 `/var/www/
  index.html` 200 返回、缺失文件 404；新增 `/var/www/index.html` 演示页与
  Makefile ramdisk 重建依赖（`$(wildcard rootfs/var/www/*)`）
- **shell `reboot`**：PSCI SYSTEM_RESET 重启（QEMU `-no-reboot` 下等价退出）
- **shell `poweroff`**：经 PSCI SYSTEM_OFF（HVC conduit）关机——QEMU virt
  下实测 `-no-reboot` 干净退出（exit=0）
- **shell `calc`**：整数算术求值器（递归下降，支持 + - * / % 与括号、
  一元负号、优先级）——实测 `calc (2+3)*4` → 20
- **TCP 服务器并发验证**：5 并发连接各自收发 + 顺序槽位回收均 PASS
- **shell `udp_echo` / `sort`**：UDP 回显服务器（hostfwd 实测 PASS）；
  `sort` 管道排序（`cat f | sort`）
- **shell `nslookup`**：DNS 域名解析（QEMU slirp DNS）——实测 `example.com` →
  `172.66.147.243`、`localhost` → `127.0.0.1`
- **shell `netstat`**：套接字表显示（FD/协议/状态/端口/远端），实测显示 LISTEN
- **`Sink::flush_uart`（阻塞命令输出修复）**：tcp_echo/http_serve 等进入事件循环
  前立即刷出缓冲输出（此前缓冲要等命令返回才输出，阻塞型命令永不显示）
- **★ HTTP 服务器（shell `http_serve`）**：极简静态页面服务器——宿主机
  curl/urllib 实测 status=200，完整返回 HTML（经 QEMU hostfwd）
- **shell `tcp_echo`**：TCP 回显服务器内置命令（配合 hostfwd 演示）
- **shell `ls` 字母排序 / `sysinfo` MemFree**：目录项按字节序排序输出；
  `sysinfo` 增加 MemFree 显示
- **shell `df`**：列出已挂载文件系统（设备/挂载点/容量/已用/空闲，位图实时统计）
- **shell `grep`**：子串过滤 + `-n` 行号，支持管道（`ls /bin | grep -n lau`）
- **eret 防御护栏（残余 eret-to-0 收口）**：`syscall_return_restore` 校验陷阱帧
  elr 非 0 且低于 0x40000000，损坏时走 `el0_fault` 干净终止进程——不再出现
  「EL0 取指 PC=0」的原始故障（多轮冒烟 0 复现）
- **shell `;` 命令分隔符 / `echo -n` / `env`**：`a; b` 顺序执行；`echo -n` 不换行；
  `env` 列出环境变量；Tab 补全识别 `;` 词边界
- **★ shell Tab 补全**：命令位置按内置命令 + /bin 补全（`ca`→`cat`）；
  路径位置支持 `/` 前缀（`ls /b`→`ls /bin`）、唯一匹配自动补齐、多候选
  换行列表示
- **shell `cd -` / `ls -a` / `ls -la`**：`cd -` 回到上一目录（OLDPWD）；
  `ls` 支持组合标志（-la），显示 `.`/`..` 与类型+大小
- **shell `history` / `uptime`**：`history` 列出命令历史；`uptime` 显示运行时间；
  `wc` 支持管道 stdin（无文件参数时统计输入流）
- **shell `cat -n` / `head`**：`cat -n` 逐行编号（含管道 stdin）；`head [-n N]`
  输出前 N 行（配合管道使用）
- **任务槽耗尽优雅降级（防 fork 炸弹 DoS）**：`alloc_tid` 不再 panic，
  `spawn`/`insert` 槽位不足返回 `u32::MAX`；`fork_parent`/`clone_parent`/
  `process::spawn`/`elf::spawn` 全部处理失败并回滚（回收子栈、地址空间与 fd 引用）
- **shell 新命令**：`ping <ip>`（内核网络栈 ICMP，实测可达 QEMU 网关）、
  `wc [file...]`（行/词/字节统计）、`ls -l`（类型 + 大小）
- **fork/clone 子进程栈布局重构（消除残余 eret-to-0）**：子进程/子线程内核栈不再
  整体拷贝父栈活动区，而是只把 272 字节系统调用陷阱帧拷到**子栈顶部**——子进程
  从 `syscall_return_restore` 直接 eret，不需要父调用链帧；原方案子进程可用栈只剩
  `copy_len-368` 字节，execve（argv/envp 数组 + setup_stack 2KB 帧）溢出踩坏
  陷阱帧 elr → 偶发 eret 到 PC=0 的 EL0 故障。重构后子进程获得完整栈空间


### 动画系统（对标 iOS / Android）

- **内核动画引擎**（`kunxcore/src/anim.rs`）：iOS Core Animation 与 Android Material
  缓动曲线（三次贝塞尔求解：牛顿 + 二分）、阻尼谐振子弹簧物理（iOS dampingRatio /
  Android stiffness 双风格，半隐式欧拉积分）、定长槽位动画管理器（窗口位置 / 透明度 /
  缩放补间与弹簧），定时器驱动 `frame_tick`，启动自检断言曲线采样值与弹簧收敛
- **合成器缩放**（`kunxcore/src/compositor.rs`）：窗口 `scale` 字段（定点 1.0 = 256）、
  以窗口中心为锚点的最近邻缩放合成、`set_scale`
- **定时器多回调**（`kunxcore/src/timer.rs`）：支持调度 tick 与动画帧 tick 并存
- **launcher 动效**（`apps/launcher/src/main.rs`）：图标按压反馈（0.85 缩放，
  FastOutSlowIn 120ms + 弹簧回弹）、应用启动转场（以图标为中心放大 1.12 + 白闪，
  EaseInEaseOut 300ms）、应用返回转场（快照缩放回桌面，EaseInEaseOut 280ms）
- **规范文档**：`docs/animations.md` 记录全部曲线 / 弹簧参数与转场规格

## [v0.1.0] - 2026-08-16

KunxOS 首个里程碑版本：一个面向 QEMU `virt`（ARM Cortex-A72）与 Raspberry Pi 4 的 Rust 裸机操作系统，具备内核、引导、用户态 init、系统服务、图形桌面、应用与包管理完整闭环。

### 内核 (kunxcore)

- **启动与内存**：MMU 4KB 恒等映射、`SCTLR_EL1.M` 使能；位图物理页分配器；slab 内核堆分配器；虚拟内存映射（`vm`）
- **进程与调度**：用户态进程 / 线程模型、上下文切换、CFS 调度器（`sched_cfs`）
- **系统调用**：Linux AArch64（asm-generic 编号）系统调用表、ELF 用户程序加载器
- **文件系统**：VFS 抽象、简单文件系统（`fs_simple`）、devfs（`/dev/fb0`、`/dev/input/event0`、`/dev/tty`）、virtio-blk 块设备、SFS 数据持久化（`/data`）
- **网络**：virtio-net 驱动、网络协议栈（`net_stack`）、socket / IPC
- **输入**：输入子系统（`input`、`input_stack`）、触摸事件处理
- **图形**：framebuffer 初始化（`gpu_simple`）、窗口合成器（`compositor`）、UI 框架（`ui_framework`）
- **兼容层**：Linux 兼容层（`linux_compat`）、bionic shim（`bionic_shim`）
- **其他**：FDT 设备树解析、GICv2 中断、PL011 UART、ARM Generic Timer、交互式 Shell（`shell`）、内置包管理（`pkg`）

### 引导 (kunxboot)

- 从 virtio-blk 读取 `KERNEL.IMG` / `KUNXOS.DTB` / `RAMDISK.IMG` 并跳转内核
- MBR + FAT32 文件系统解析
- SD / MMC 驱动
- DTB 解析
- Raspberry Pi 4 后端（DDR 初始化、Pi4 链接脚本）

### 硬件抽象层 (kunxhal)

- 平台抽象：display / input / audio / net / storage / power
- QEMU 与 Raspberry Pi 4 双平台后端

### init (kunxinit)

- EL0 用户态 init（PID 1 语义由内核承载）
- `/etc/inittab` 解析，支持 sysinit / once / wait / respawn
- 崩溃服务按节流策略自动重启

### 系统服务 (kunxserv)

- **networkd**：网络守护进程，状态查询与命名管道命令接口（STATUS / RENEW）
- **displayd**：显示守护进程（已废弃，launcher 直接操作 `/dev/fb0`）

### 应用 (apps)

- **launcher**：桌面启动器（状态栏、应用网格、底部 Dock、窗口 z-order 管理）
- **oobe**：首次开机向导（欢迎 → 语言 → 模块 → 地区/时区 → 网络 → 协议 → 账号 → 安全 → 主题 → 完成，共 10 步）
- **settings**：系统设置（恢复已移除模块）
- **terminal** / **calculator**：应用骨架（验证窗口生命周期）
- **store**：应用商店骨架（软件目录统计）

### 包管理 (kunxpkg)

- 命令：`pkg update | install <name> | remove <name> | list | search <kw> | info <name>`
- `.pkg` 包格式（NOVAPKG1 魔数 + FNV-1a 签名），本地 `file://` 软件源

### 工具 (tools)

- `mkdisk.py`：生成磁盘镜像（MBR + FAT32 + SFS rootfs + SFS data）
- `mkfs_sfs.py`：SFS ramdisk 打包
- `input_inject.py`：触摸输入注入
- `release.sh`：版本发布脚本

### 构建

- 顶层 Makefile：`kernel` / `init` / `networkd` / `rootfs` / `run` / `boot` / `pi4` 等目标
- QEMU virt 与 Raspberry Pi 4 双平台支持

### 新增：电源与热管理子系统（HAL 抽象 / DVFS / 热节流 / Idle 状态机 / 电池模型 / 后台冻结）（2025-08-27 轮）

- **HAL 电源抽象**（`kunxhal/src/power.rs`）：新增 `PowerControl`（关机/重启/休眠）、
  `ThermalSensor`（毫摄氏度）、`DvfsController`（按簇频点表 + 频率读写，预留
  big.LITTLE）、`Battery`（电量/充电）；`Platform` trait 增加 5 个默认 `None`
  的电源访问器（`power_control` / `thermal_sensor` / `dvfs_controller` /
  `battery` / `core_voltage_mv`）。
- **QEMU 后端**（`plat_qemu.rs`）：PSCI `hvc` 关机/重启；虚拟 DVFS 控制器
  （频点表 600/900/1200/1500 MHz 真实、切换为记账），governor/热节流决策
  可端到端验证；温度与电池返回 `None`（内核用模拟）。
- **Pi4 后端**（`plat_pi4.rs`）：PSCI `smc` 关机/重启；mailbox 属性通道
  `GET_TEMP`（0x00030006，SoC 温度，毫摄氏度）；`GET/SET_CLOCK_RATE`
  （0x00030002/0x00038002，ARM 时钟 id 0x3）DVFS；`GET_VOLTAGE` 仅读取
  （v1 不切压，避免真机风险）。
- **内核策略引擎**（`kunxcore/src/power.rs`，平台无关）：
  - CPU DVFS：100ms 负载采样（每核 idle 任务 WFI 记账），ondemand /
    conservative / powersave / performance 四 governor，迟滞 + 100ms 最小
    切换间隔防抖，切换日志 `[power] dvfs ... Hz (load=..%, governor=..)`。
  - 热管理：200ms 采样，五级节流（70/80/85/90°C → warm/hot/critical/
    emergency）；Hot 压到 900MHz、Critical+ 压到最低频（force 绕过切换
    间隔）；QEMU 软件热模型（满载稳态 85°C，可 `power thermal force` 注入）。
  - Idle 状态机：每核 active/idle/wfi 阶段 + WFI/唤醒计数（`enter_wfi`/
    `exit_wfi`），为负载采样与空闲统计提供数据；深空闲为预留接口。
  - 电池模型框架：充电状态机（discharging/charging/full）+ 事件通知
    （低电量 ≤15% 迟滞复位 / 充满 / 插拔），虚拟电池供 QEMU 测试，
    HAL 电池为真机预留。
  - 后台冻结（Doze）：CFS 每任务 `cpu_cap` 0..100（vruntime ×100/cap
    限速，linux cpu.max 思路）+ 软冻结（cap=0 不被选中），与既有 SIGSTOP
    互补。
- **调度器**（`sched_cfs.rs`）：`Task.cpu_cap` 字段；tick/yield_now 按
  cap 放大 vruntime；`pick_next` 跳过冻结任务；idle 任务 WFI 记账；
  `set_task_cpu_cap` / `for_each_task_cap`。
- **控制面**：shell `power` 命令（status/governor/thermal force/battery/
  plug/cap/freeze/unfreeze/selftest）；`/proc/power`、`/proc/thermal`
  （可读 + 可写控制）；syscall 410-413（`kunx_pm_get` 打包状态 /
  set_governor / set_cpu_cap / freeze）。
- **测试**：boot 期自动运行电源逻辑自检（QEMU/Pi4 启动即验证 26 项）；
  `tools/power_test.py` + `make power-test`（QEMU 回归：selftest 0 failure、
  热注入节流降频、governor 切换、电池状态机、procfs，21/21 通过）。
  详见 `docs/power_management.md`（含 Pi4 mailbox 协议、QEMU 热模拟方案、
  DVFS 对 120FPS 影响分析、真机压力测试步骤）。
