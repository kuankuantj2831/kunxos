# KunxOS 无线协议栈：架构、状态机、配置存储与测试方案

> 本文档对应 `wireless/`（crate 名 `kunxwireless`）—— 供内核（kunxcore）、
> 系统守护进程（wifid / btd / locationd）与宿主单元测试三处复用的 **no_std 无线
> 协议库**。目标平台：QEMU `virt`（逻辑/模拟测试）与 Raspberry Pi 4
> （CYW43455 Wi-Fi+BT combo：Wi-Fi 走 SDIO、蓝牙走 UART）。
>
> 配套文档：内核侧 `/dev/wifi` 驱动与 Pi4 真机接线清单见 `docs/wifi.md`。

---

## 1. 无线架构图

```
 桌面应用（设置/状态栏）        wifid 守护进程（kunxserv/wifid）
       │ 命令/事件                    │  SCAN/CONNECT/DISCONNECT/STATUS/ROAM
       ▼                             ▼
  kunxwireless::wifi_config   kunxwireless::wifi_mgmt（状态机）
  （已知网络 + 密钥环封装）     扫描表 / 认证 / 关联 / 4-way / 漫游 / 802.1X
       │                             │ devproto 字节协议（/dev/wifi）
       ▼                             ▼
  ┌──────────────── 内核（kunxcore） ────────────────────────┐
  │  wifi.rs：/dev/wifi 字符设备 + 事件队列 + STA 侧 4-way    │
  │  net_stack.rs：IP/ARP/UDP/TCP/DNS + DHCP（RFC 2131）      │
  └──────────────┬───────────────────────────────────────────┘
                 │ kunxhal WifiDevice trait
                 ▼
  ┌─ QEMU 后端（SimWifi） ─┐        ┌─ Pi4 后端（CywWifi，见 docs/wifi.md）─┐
  │ 确定性 AP 表 + AP 侧  ─┼────→    │ dwmmc/SDIO → CMD52/CMD53 → CYW43455   │
  │ 4-way + 静态租约        │        │ 固件下载（brcm.rs 流程）→ 关联 → 数据  │
  └────────────────────────┘        └───────────────────────────────────────┘

 蓝牙（CYW43455 板上蓝牙，Pi4 走 UART H4）：
   btd 守护进程 ── HCI（hci.rs H4 帧）── CYW43455 BT 控制器
        ├── L2CAP（l2cap.rs，含 LE Credit Based）
        ├── ATT/GATT（att.rs / gatt.rs）：服务发现、特征读写、通知/指示
        ├── SMP（smp.rs）：配对与 Bonding（Just Works / NC / Passkey）
        ├── RFCOMM（rfcomm.rs）→ HFP（hfp.rs）：通话
        └── AVDTP（a2dp.rs）→ A2DP SBC：音乐，延迟优化（eSCO/帧时长）

 定位预留：locationd ── nmea.rs（NMEA 0183）── loc.rs（Wi-Fi+基站+GPS 融合）
```

数据面：EAPOL-Key（ethertype 0x888E）经内核 `/dev/wifi` 中继给 STA 侧 4-way；
连接成功后由 DHCP（真实）或静态租约（QEMU 模拟）配置 IP，`networkd` 的
`STATUS` 返回真实链路信息。

---

## 2. Wi-Fi 扫描与连接状态机

与 `devproto` 的 `/dev/wifi` 状态对齐：`Idle → Scanning → Connecting →
Handshake → Connected`。库内阶段（`wifi_mgmt::ConnectPhase`）：

```
              WPA2-PSK   ┌─→ Auth(open) ─→ Assoc ─→ 4-way → Connected
connect ──→└─ 开放网络   ─→ Auth(open) ─→ Assoc ────────→ Connected
              WPA3-SAE   ─→ SaeCommit ↔ SaeConfirm ─→ Assoc ─→ 4-way(PMK=SAE-PMK)
              802.1X     ─→ Auth(open) ─→ Assoc ─→ EAP(Identity/MD5/TLS)
                             └─ EAP-Success → MSK → PMK → 4-way
```

- **SSID 发现**：`WifiSta::scan` / `scan_result`（QEMU 模拟后端与真实驱动统一注入）/
  `pick_best_bss` 选信号最强；`scan_done` 自动连接已配置网络。
- **认证**：开放系统 `AUTH_OPEN`；SAE commit/confirm 96/32 字节载荷由
  `sae.rs` 计算，驱动编帧到 802.11 Auth 帧。
- **关联**：`on_auth_result` → `on_assoc_result`；RSN IE 经 `ie::build_rsn_ie`。
- **4-way handshake**：`on_eapol_key` 驱动 `eapol::FourWay`（Msg1→Msg2，
  Msg3→安装 GTK→Msg4），WPA2 由 PSK→PBKDF2→PMK；WPA3 用 SAE 派生的 PMK；
  802.1X 用 EAP-TLS 的 MSK 前 32 字节。
- **断开/重连**：`disconnect` / `on_disconnected`（reason 驱动）。
- **基础漫游**：`update_rssi`（低于 RSSI_ROAM_THRESHOLD 且存在更优 BSS）→
  `roam_to` 重连。**802.11r FT 不在初始范围**（见 §6）。

---

## 3. 配置管理（已知网络 / 密码/证书安全存储）

`wifi_config.rs`：

- `WifiProfile`：SSID、认证方式（`devproto` WIFI_AUTH_*）、隐藏、自动加入、
  优先级；PSK 与 EAP 口令/证书引用。
- `ProfileStore`：固定数组（≤16），`upsert/find/remove/auto_join_target`。
- 序列化：定长二进制（`serialize_store` / `deserialize_store`），供 `/data/wifi.conf`
  落盘。
- **KeyStore 集成**：`seal_store` / `open_store` —— AES-KW（RFC 3394）用主密钥
  包装 DEK，DEK 加密整个载荷（AES-CTR，12 字节 nonce），SHA-256 前 8 字节做
  完整性标签；主密钥由 `kunxsec` KeyStore 供给（本库纯函数、传入主密钥）。
  篡改载荷或错误主密钥均被拒绝（`open_store` 返回 `None`）。

---

## 4. 蓝牙协议层

### 4.1 HCI（`hci.rs`）
H4 帧：命令 `[0x01 opcode len params]`、事件 `[0x04 code len params]`、
ACL/SCO；LE 命令构造（扫描/连接/广播/连接更新）与 LE Meta 事件解析
（Adv Report / Connection Complete）。Pi4 蓝牙走 UART（H4）或 USB（H4 子集），
传输层由 btd 后端提供（QEMU 逻辑测试在传输边界模拟注入）。

### 4.2 BLE GATT（`l2cap.rs` / `att.rs` / `gatt.rs`）
- L2CAP：基本帧（长度/CID）与 LE Credit Based 信令。
- ATT：请求/响应/错误码编解码 + 无堆服务端引擎（Exchange MTU、Find
  Information、Read By Type、Read/Blob、Write）。
- GATT：服务/特征数据库（`GattDb`）、`parse_char_decl`（16/128 位 UUID）、
  Find Information 解析 —— 覆盖服务发现、特征读/写、通知/指示（CCCD 句柄
  归属）。

### 4.3 配对与 Bonding（`smp.rs`）
- 方法选择表（Core Spec Table 2.8）：Just Works / Passkey Entry（显示/输入/
  角色）/ Numeric Comparison；
- Legacy：c1 确认值 + s1 STK；LE Secure Connections：f4/f5/f6/g2（AES-CMAC，
  与 Zephyr/规范字节序一致）与 P-256 ECDH；
- 公钥交换、确认/随机交换、DHKey Check、密钥分发（EncInfo / CentralIdent /
  IdentInfo / SigningInfo）与 `Bond` 记录。

### 4.4 经典蓝牙音频
- `rfcomm.rs`：TS 07.10 复用器（地址/控制/长度/FCS-8、PN/MSC、信用流控），
  HFP 传输通道；
- `hfp.rs`：AT 命令子集（BRSF/CIND/CMER/CLCC/VGS + 应答/挂断/拨号）、AG 响应
  解析（OK/ERROR/BRSF/CIND/CIEV/RING）与 eSCO（S3 低延迟）参数 + HCI
  Enhanced Setup Synchronous Connection；
- `a2dp.rs`：AVDTP 信令（Discover/GetCaps/SetConfig/Open/Start/Close/Suspend）、
  SBC 编解码器能力与 RTP+SBC 媒体包；`estimated_media_latency_ms` 与 eSCO
  延迟估算用于**延迟优化**。

---

## 5. 测试方案

### 5.1 QEMU 逻辑测试（已随 `make test-wireless` 运行）
宿主 `cargo test`（纯逻辑，无硬件）：

| 模块 | 用例 | 说明 |
| --- | --- | --- |
| wifi_mgmt | `scan_pick_best_bss` / `open_network_connect` / `wpa2_psk_four_way` / `wpa3_sae_via_manager` / `enterprise_eap_md5_flow` / `basic_roaming_decision` | **模拟扫描结果注入** + 模拟 AP（用同库 eapol/sae/eap 做 AP 侧），全链路：扫描→认证→关联→4-way/SAE/802.1X→连接→漫游 |
| wifi_config | `store_add_find_remove` / `serialize_roundtrip` / `seal_open_roundtrip_and_tamper` | 配置文件与 KeyStore 封装/解封/篡改拒绝 |
| rfcomm / hfp / a2dp | 帧编解码、PN/MSC、eSCO 参数、AVDTP 头、SBC 能力、RTP 媒体包、延迟估算 | 蓝牙经典音频信令 |
| smp | `full_sc_just_works_pairing` / `full_sc_numeric_comparison` / `full_sc_passkey_entry` / `legacy_pairing_just_works` | 配对双方（发起方×应答方）全流程：公钥→确认/随机→DHKey Check→LTK 一致 |
| brcm | `crc7_known_vector` / `cmd52_cmd53_framing` / `r5_parse` / `window_math` / `upload_plan_and_loader` | SDIO 传输原语与固件/NVRAM 上传计划（协议级） |

新增模块已加入 `.github/workflows/ci.yml` 的宿主测试清单与 `Makefile`
`test-wireless` 目标。

### 5.2 Pi4 真机连接测试
按 `docs/wifi.md` §7 接线：
1. **CCCR 探测**（无需固件）：CMD52 读 CCCR 制造商/产品应返回 CYW43455；
2. **固件下载**（`brcm.rs` 流程 + `docs/wifi.md` 时序）：
   唤醒 F1 → 设块大小 → 背板窗口上传固件镜像 → NVRAM 至 RAM 尾 → RAMEND 指针
   → 启用 F2 → 读回验证；
3. **扫描/关联/EAPOL**：WLC_SCAN → bss_info → WLC_JOIN → 0x888E 中继到
   本库 STA 4-way；
4. **验证点**：`make pi4` 编译检查、SDIO CMD52/53 帧（`brcm.rs` 已测）、
   4-way 密码学（QEMU 已测）。

---

## 6. 关键问题答复

1. **CYW43455 固件加载流程（SDIO）当前是否已打通？**
   **未打通。** 内核此前无 SDIO/MMC 控制器驱动，Pi4 后端为骨架；本次在协议库
   提供可单测的 SDIO 传输原语（CMD52/53 + CRC-7 + R5）、背板窗口编址与固件/
   NVRAM 上传计划（`brcm.rs`），并把下载流程固化进 `FwLoader` 状态机。真机
   打通点是 SDIO 控制器驱动 + 固件 blob 注入，见 `docs/wifi.md` §7。

2. **是否接受绑定开源蓝牙栈（Zephyr BT / NimBLE）以减少自研量？**
   **不绑定完整栈。** 现有实现已自研覆盖 HCI/L2CAP/ATT/GATT/SMP/RFCOMM/HFP/
   A2DP 全链路（本库可测、QEMU 可回归），符合“不引入巨大 C 代码量、优先自研
   核心逻辑”的约束。只在**传输层**保留后端绑定点（Pi4 的 UART-H4 / 模拟的
   注入测试），不依赖 Zephyr/NimBLE。

3. **802.11r 漫游是否在初始范围？**
   **不在初始范围；先实现基础重连漫游。** `wifi_mgmt::update_rssi`（信号低于
   阈值且存在更优 BSS）→ `roam_to`（对目标 BSS 重新关联 + 4-way）。802.11r
   FT（Fast Transition：over-the-air / over-the-DS）列为后续工作，接口预留
   （`ConnectPhase` 与 `RoamDecision` 已可扩展）。