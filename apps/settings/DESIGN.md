# KunxOS 设置应用：架构与设计

对标 iOS「设置」/ Android「设置」的完整设置体验：主页面 → 分类子页 → 具体设置项
（开关 / 滑块 / 选择器 / 输入框 / 确认框），顶部全局搜索入口；配置落盘
`/data/settings/<分类>.conf`（手写 `key=value`），修改即时生效，并通过既有
子系统消费端（`/data/oobe/*`、`/var/run/wifid.pipe`、电源 syscall 411、
`/data/settings/notify` 变更标记）下发，服务端按文件监听即可感知变更。

实现位置：`apps/settings/src/`（no_std、无堆、裸 syscall，AArch64）。

| 模块 | 职责 |
|------|------|
| `main.rs` | 应用外壳：syscall 封装、framebuffer/输入轮询、主循环、光标 overlay、导航栏、toast |
| `cfg.rs` | 配置存储：KV 缓冲、`/data/settings/*.conf` 读写、默认值种子、发布（mirror/pipe/syscall） |
| `ui.rs` | 组件库：毛玻璃面板、列表行、开关、滑块、单选、步进、按钮、渐变背景、图标 |
| `pages.rs` | 页面模型：页栈、滚动/滑块抓取、搜索索引、主/显示/声音/通知/安全/输入/图案/确认页、命中与动作 |
| `pages2.rs` | 网络/蓝牙/移动数据/VPN/NFC/勿扰/权限/账户/云/系统/关于/更新/时间/语言/开发者/电池/存储/辅助功能页 |

---

## 1. 设置架构：分类模型、配置项定义、默认值

### 配置存储约定

- 根目录 `/data/settings/`，每分类一个文件，`key=value` 一行一条（手写格式，无依赖库）。
- 启动时 `settings_init()` 建目录并加载全部文件；文件缺失用默认值种子并立即落盘。
- 每次变更走统一入口 `cfg::cfg_set(cat, key, val)`：内存缓冲更新 → 落盘 →
  写 `/data/settings/notify`（`<分类> <key>`，最新覆盖）→ `apply` 镜像到子系统消费端。

### 分类文件与默认值

| 文件 | 键 | 默认 | 说明 |
|------|-----|------|------|
| `network.conf` | `wifi_enabled` `bt_enabled` `data_enabled` `vpn_enabled` `nfc_enabled` | 0×5 | 网络开关 |
| | `wifi_ssid` `wifi_psk` | 空 | 手动连接参数 |
| | `ip_mode` | `dhcp` | dhcp / static（静态配置落盘，内核接入预留） |
| | `ip_addr` `ip_gw` `ip_dns` | `192.168.1.100` `192.168.1.1` `10.0.2.3` | 静态 IP |
| `display.conf` | `brightness` | 85 | 亮度 0..100，launcher 同文件 |
| | `font_size` | 1 | 0 小 / 1 标准 / 2 大 / 3 超大（本应用内即时缩放） |
| | `dark_mode` | 1 | 镜像 → `/data/oobe/theme`（保留 wallpaper 后缀） |
| | `resolution` | `540x960` | 固定（显示模式预留） |
| | `refresh_rate` | 120 | 60/120（合成器预留） |
| | `auto_rotate` | 0 | 传感器旋转预留 |
| `sound.conf` | `vol_media` `vol_ring` `vol_alarm` `vol_call` | 60 | 四路音量滑块；媒体音量镜像 → `/data/oobe/volume` |
| | `vibration` | 1 | 振动开关（预留） |
| | `ringtone` | `classic` | classic / pulse / dual（合成铃声选择） |
| | `dnd_enabled` `dnd_start` `dnd_end` | 0, 22, 7 | 勿扰 + 时段（依赖 lifed，预留） |
| `notifications.conf` | `push_enabled` | 1 | 推送服务开关（pushd 对应） |
| | `preview` | `full` | full / hidden / off（锁屏预览） |
| | `app_<name>` | `banner` | 每应用样式 banner / silent / off |
| `security.conf` | `lock_type` | 继承 /data/oobe | slide / pin / pattern / password |
| | `lock_hash` | 继承 | FNV-1a32 8hex（与 launcher 校验一致） |
| | `duress_hash` `duress_action` | 继承, `fake_system` | 胁迫 PIN（已存在，读写 /data/oobe/duress_*） |
| | `location` | 1 | 位置服务开关（positiond 预留） |
| | `ad_id` | 时间派生 | 广告标识符（可重置） |
| `accounts.conf` | `device_name` | `KunxPhone` | 设备名称 |
| | `local_user` | 继承 /data/oobe | 本地账户用户名（改名保留密码哈希） |
| | `cloud_enabled` | 0 | 镜像 → `/data/oobe/cloud_enabled` |
| | `cloud_account` | 继承 | `<user>:<fnv1a64>`（登录写回 /data/oobe/cloud_account） |
| | `sync_enabled` | 1 | 账户同步（预留） |
| `system.conf` | `lang` | 继承 /data/oobe/lang | zh_CN / en_US（写回） |
| | `timezone` | 继承 | IANA 时区（写回，时区表见 shared/tz.rs） |
| | `auto_time` `fmt_24h` | 1, 1 | 自动时间 / 24 小时制 |
| | `keyboard` | `builtin` | 输入法选择（IME 预留） |
| | `usb_debug` `oem_unlock` | 0, 0 | 开发者选项（预留） |
| `battery.conf` | `show_percent` | 1 | 电量百分比显示 |
| | `low_power` | 0 | 低电量模式：开 → syscall 411 powersave，关 → ondemand |
| `storage.conf` | `cache_cleaned` | 0 | 缓存清理计数 |
| `accessibility.conf` | `magnifier` `invert_colors` `high_contrast` | 0 | 放大器（预留）/ 颜色反转 / 高对比度（后两者 flip LUT 即时生效） |

### 发布映射（实时生效路径）

| 键 | 目标 |
|----|------|
| `brightness` | 写 `/data/oobe/brightness`；本应用 flip 像素 LUT 缩放 |
| `dark_mode` | 镜像 `/data/oobe/theme`（保留 `wallpaper:N`） |
| `vol_media` | 写 `/data/oobe/volume`（launcher 主音量） |
| `wifi_enabled` | 开 → `/var/run/wifid.pipe` `AUTO`；关 → `DISCONNECT` |
| `lang` | 写 `/data/oobe/lang` |
| `timezone` | 写 `/data/oobe/timezone` |
| `lock_type` / `lock_hash` / `duress_*` | 写 `/data/oobe/security_{type,hash}` / `duress_{hash,action}`；lock_hash 同步 `local_account` 哈希段 |
| `local_user` | 重写 `/data/oobe/local_account` 用户名列（保留哈希） |
| `cloud_enabled` / `cloud_account` | 写 `/data/oobe/cloud_enabled` / `cloud_account` |
| `low_power` | syscall 411 切 governor（powersave / ondemand） |
| 其余 | 仅落盘 + `/data/settings/notify` 标记（子系统接入预留） |

---

## 2. 每类设置页：UI 布局、交互逻辑、配置读写

页面模型：静态页栈（深度 ≤ 8，`PAGE_STACK`/`PAGE_DEPTH`）+ 每页滚动（`SCROLL` +
渲染时写 `CONTENT_H` → `max_scroll`）。返回按钮 / Esc / 导航返回逐级退栈，首页退栈即退出。

通用组件（`ui.rs`）：毛玻璃卡片行（图标 + 标题 + 右侧值/箭头/开关）、开关、滑块
（点击/拖拽改值）、单选圈、步进 `- / +`、主/危险/普通按钮、分组标题、渐变背景、
数字键盘、3×3 图案网格、确认框。

| 页 | 内容 | 交互 / 读写 |
|----|------|-------------|
| 主页面 | 10 分类 + 顶部搜索栏 | 点分类进入子页；搜索栏进入输入页，键入即全局过滤 |
| 网络与连接 | Wi-Fi / IP 设置 / 蓝牙 / 移动数据 / VPN(预留) | Wi-Fi 进入专页；IP 进入设置页；开关落盘 |
| Wi-Fi | 开关、扫描、手动连接、自动连接、已保存清单 | 开关发 wifid `AUTO/DISCONNECT`；扫描发 `SCAN`；已保存网络点击 → `CONNECT <ssid>`；手动连接 → SSID/密码输入页 → `CONNECT ssid psk` |
| IP 设置 | DHCP/静态单选 + 地址/网关/DNS 输入 | 切模式；输入页 Enter 落盘（内核 net_stack 接入预留） |
| 显示 | 亮度滑块、深色、字体大小、分辨率(固定)、刷新率、自动旋转 | 亮度拖拽即时 LUT；深色/刷新率/旋转切值并发布 |
| 声音 | 四路音量滑块、铃声、振动、勿扰 | 滑块即时落盘；铃声循环 经典/脉冲/双音；振动开关 |
| 勿扰 | 开关 + 开始/结束时段步进 | 时段 0..23 加减，落盘（lifed 接入预留） |
| 通知 | 推送开关、锁屏预览(循环)、应用通知管理 | 预览 full→hidden→off；每应用样式 banner→silent→off |
| 安全与隐私 | 锁屏方式(循环 slide→pin→pattern→password)、修改 PIN/密码/图案、胁迫 PIN 与动作、应用权限、位置、重置广告 ID | 锁屏方式循环写 lock_type；PIN/密码 数字键盘+物理键盘，旧值校验(FNV)后写 hash+type；图案 3×3 网格（≥4 点）；胁迫 PIN 数字输入 + 动作单选；权限页见下；位置开关；广告 ID 以时间派生新值 |
| 应用权限 | 每应用清单 → 权限详情 | 读 `/var/lib/pkg/permissions/<app>`；点权限行即撤销（删行重写） |
| 账户 | 设备名称、本地账户、账户同步、云服务 | 名称/用户名输入页落盘并同步 /data/oobe；同步开关；云进入登录页 |
| 云服务 | 登录 / 退出 | 登录输入 用户名+密码 → `token = fnv1a64(user:pass:kunxos-cloud)` 写 cloud_account + 开 cloud_enabled（与 cloud 应用一致） |
| 系统 | 关于 / 软件更新 / 日期与时间 / 语言 / 键盘(IME 预留) / 导航方式 / 应用管理 / 重置 / 开发者选项 | 导航方式写 /data/oobe/nav_mode；应用管理 → 恢复已移除模块（复用原逻辑） |
| 关于 | 设备名称、型号、系统版本、内核版本、构建号 | 静态展示 + 存储统计 |
| 软件更新 | 检查更新 / 安装 / 查看状态 / 重启 | 扫描 `/data/ota/*.kxup`；检查=syscall 430 cmd2、安装=cmd1、状态=cmd3；重启经确认框 → syscall 142 |
| 日期与时间 | 自动设置、时区(循环 TZDB)、24 小时制 | 时区经 shared/tz.rs 索引循环切换并写回 |
| 语言 | 简体中文 / English | 写 /data/oobe/lang（多语言预留） |
| 开发者选项 | USB 调试、OEM 解锁 | 落盘预留 |
| 电池 | 电量状态(syscall 410)、电量百分比、低电量模式 | 状态实时读 410 打包位；低电量切 governor |
| 存储 | 总/已用/剩余(statfs /data)、使用率条、清除缓存 | 缓存遍历 `/data/app/<name>/cache/` 顶层文件 unlink，计数 toast |
| 辅助功能 | 放大器 / 颜色反转 / 高对比度 | 反转/对比度 flip LUT 即时生效，落盘 |
| 输入页 | 通用输入（搜索/SSID/密码/IP/用户名/设备名/云账号） | 物理键盘键入，Enter 确认，Backspace 删除，Esc 返回；PIN 类附屏幕数字键盘 |
| 确认页 | 还原设置 / 恢复出厂 / 重启 | 「还原」重建全部配置默认值；「恢复出厂」syscall 430 cmd7 清除 /data 后重启（重走 OOBE）；「重启」syscall 142 |

---

## 3. 配置管理：加载 / 保存 / 默认值 / 迁移

- **加载**：`settings_init` 建目录 → `cfg_load` 逐个读入分类缓冲（缺失 len=0）。
- **保存**：`cfg::cfg_set` 唯一写入口（内存更新 + 落盘 + notify + apply）。
- **默认值**：`seed_all` 对 len=0 的文件逐键种子（首启建文件）。锁屏/语言/时区/账户
  等从既有 `/data/oobe/*` 继承（迁移），不覆盖已有配置。
- **迁移**：`/data/oobe` → `/data/settings` 的单向导入发生在首启种子；此后
  `/data/settings` 为规范源并反向镜像到 `/data/oobe`（launcher/锁屏仍读旧路径，
  保证兼容）。

## 4. 服务通知

- `/data/settings/notify`：每次变更写 `"<分类> <key>"`（最新覆盖），服务端
  轮询/监听该文件即可感知（文件监听方案）。
- `/var/run/wifid.pipe`：Wi-Fi 动作真实下发（SCAN/CONNECT/AUTO/DISCONNECT）；
  wifid 每 250ms 轮询清空。
- syscall 411：低电量模式 ↔ governor；syscall 142：重启；syscall 430：OTA/恢复出厂。
- `/data/oobe/*`：launcher 锁屏/主题/导航/时钟实时读取，设置写回即生效。
- 预留项（蓝牙/移动数据/VPN/NFC/勿扰/权限/存储统计等）仅落盘 + notify，
  待对应守护进程按文件接入。

## 5. 搜索（全局设置项搜索）

主页面顶部搜索栏 → 输入页（`Ctx::Search`，物理键盘键入）；`SEARCH_LIST` 静态索引
覆盖全部主要设置项（标题 + 目标页/子页/输入上下文）；根页面在 `SEARCH_LEN>0` 时
按标题子串过滤渲染结果行，点击直接进入对应子页；✕ 或清空还原分类列表。

---

## 关键问题回答

1. **UI 框架状态**：此前只有一个静态首页 + 4 开关；本次新增毛玻璃卡片、列表行、
   开关、滑块、单选、步进、数字键盘、图案网格、通用输入页、确认框与页栈导航。
2. **硬件可用性**：亮度/深色/反色/对比度/字体为本应用内 LUT（launcher 同款亮度
   文件）；Wi-Fi 真实可用（wifid 管道 + /data/wifi 配置）；锁屏 hash 与 launcher
   实时校验；电池经 syscall 410；时区/语言/导航写回 /data/oobe。蓝牙/移动数据/
   VPN/NFC/勿扰/分辨率/刷新率/自动旋转/IME/USB 调试等为预留（无硬件或未接入）。
3. **变更通知机制**：文件监听（`/data/settings/notify` + `/data/oobe/*`）+ 管道
   （wifid.pipe）+ 信号式 syscall（电源/重启/OTA），无需新内核 IPC。
4. **恢复出厂设置**：syscall 430 cmd7 递归删除 `/data` 普通文件（保留目录、删除
   `.done`）→ 立即重启 → 重跑 OOBE（与 recovery 菜单同一内核路径）；「还原设置」
   仅重建 `/data/settings/*` 与导航默认值，不清数据。