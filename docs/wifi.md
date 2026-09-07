# KunxOS Wi-Fi 管理：简化版扫描 / WPA2-PSK 连接 / 状态机

> 目标平台：Raspberry Pi 4（CYW43455，SDIO）与 QEMU `virt`（软件模拟后端）。
> 语言：Rust。初始范围：**WPA2-PSK only**（WEP / WPA3-SAE / 802.1X 不在本版本范围，
> 扫描结果中识别但不支持）。
> 状态：QEMU 模拟后端**完整可用**（真实 4-way 密码学链路可验证）；Pi4 真机后端为
> **骨架**（SDIO 寄存器 / 固件下载 / WLC ioctl 常量已就绪，硬件接线待真机验证）。

---

## 1. 架构总览

```
                  wifid（用户态守护进程，kunxserv/wifid）
                  ├─ 状态机：Idle→Scanning→Associating→Authenticating→Connected→Disconnected
                  ├─ 配置存储：/data/wifi/networks/<ssid>.conf（ssid/psk/auth/priority）
                  └─ 命令入口：/var/run/wifid.pipe（SCAN/CONNECT/SAVE/FORGET/LIST/AUTO/STATUS/DISCONNECT）
                          │  devproto 字节协议（kunxwireless::devproto）
                          ▼
                  /dev/wifi（内核字符设备，kunxcore/src/wifi.rs）
                  ├─ 命令→事件：SCAN/CONNECT/DISCONNECT/STATUS + ioctl(TRIGGER_SCAN/GET_STATE)
                  ├─ 扫描缓存 + 事件队列（SCAN_RESULT/SCAN_DONE/CONNECTED/.../ERROR）
                  ├─ WPA2-PSK 4-way handshake（STA 侧，kunxwireless::eapol::FourWay）
                  │   PSK+SSID → PBKDF2(4096) → PMK → Msg1/2/3/4 与 AP 交换
                  └─ 连接成功后：静态租约（QEMU 模拟）或线上 DHCP（net_stack::dhcp_configure）
                          │  WifiDevice trait（kunxhal/src/wifi.rs）
                          ▼
                  Wi-Fi 后端：
                  ├─ QEMU：SimWifi（kunxhal/src/plat_qemu.rs）—— 模拟 AP +
                  │        真实 AP 侧 4-way（同库密码学），错口令在 Msg2 被拒
                  └─ Pi4：CywWifi（kunxhal/src/plat_pi4.rs）—— CYW43455 SDIO 骨架
```

数据流（关联 + 握手）：

```
wifid CONNECT <ssid> <psk>
  → /dev/wifi [WIFI_CMD_CONNECT, ssid\0 psk\0 auth]
  → 内核：目标设置 → 扫描缓存定位 BSS → HAL.associate(BSSID, STA_MAC)
  → HAL 返回 Associated（真实世界：802.11 Auth/Assoc 完成）
  → 内核：PMK=PBKDF2(psk,ssid) → 生成 SNonce
  → 等 HAL.recv_eapol() 收 Msg1（ANonce）→ STA 4-way Msg2 → HAL.send_eapol()
  → AP 校验 Msg2 → 回 Msg3（含 KEK 包装的 GTK）→ STA 解开 GTK
  → STA 回 Msg4 → 双方密钥安装 → HAL 链路 Secured
  → 内核 → DHCP（真实）/ 静态租约（模拟）→ 发 [WIFI_EVT_CONNECTED]
  → wifid：状态 Connected，日志/事件可见
```

---

## 2. 关键问题答复

### 2.1 Pi4 的 CYW43455 SDIO 当前是否已打通？

**未开始。** 整个内核此前没有任何 SDIO / MMC 控制器驱动（kunxboot 的
MMC/SD 读取属于 bootloader，与内核共用结构不同；内核侧 `blkdev` 只有
virtio-blk 与 EMMC2 信息预留）。本次交付的 Pi4 后端是**可编译的骨架**：

- BCM2711 无线 SDIO 控制器 **mmc@0xFE300000**（DesignWare MMC，即
  `brcm,bcm2835-mmc`；与 SD 卡所在的 EMMC2 @0xFE340000 分属两条总线）。
- dwmmc 寄存器定义 + CMD52/CMD53 参数编码 + FIFO PIO 读写原语（`dw_read8` 等）。
- CCCR 探测 / 回板窗口常量 / 芯片核心复位常量 / SDPCM 帧头 / WLC ioctl 与事件号。
- 固件注入入口 `cyw_wifi_set_firmware` 与探测入口 `cyw_wifi::sdio_probe`。

`WifiDevice::available()` = 探测通过 && 固件已注入；`associate` 在硬件接线完成前
返回 `Err(NotImplemented)`。**真机接线步骤见 §6。**

### 2.2 是否接受绑定开源 Wi-Fi 栈（如 brcmfmac 简化版）？

**务实方案：不移植 C 代码栈，按 brcmfmac 的总线协议用 Rust 自研极简子集。**

- brcmfmac（C）体量约 4 万行且深度耦合内核 MLME；直接移植违反「不引入巨大 C
  代码量」的约束。
- 我们只保留其**总线语义**做参考实现：「回板窗口访问 → 固件下载 → SDPCM 帧 →
  WLC ioctl（UP/SCAN/JOIN/DISASSOC/GET_RSSI）→ WLC 事件环」。这些常量与字节
  布局已在 `kunxhal/src/plat_pi4.rs` 中给出，真机接线时按 `wlioctl.h` /
  `brcmfmac_sdio.c` 逐项核对。
- **注意**：CYW43455 的**固件 blob 是闭源二进制**，不能由 Rust 重写。
  固件仍必须来自官方发布：
  `brcmfmac43455-sdio.bin`（~500KB）、`brcmfmac43455-sdio.clm_blob`、
  型号 NVRAM（`brcmfmac43455-sdio.raspberrypi,4-model-b.txt`）。

### 2.3 固件 blob 如何加载？

- 方案 A（推荐真机）：从 FAT32 `/boot` 分区读取。bootloader 或内核把 FAT32 挂载到
  `/mnt/fat32`（内核 `data_boot` 已对 FAT32 分区自动挂载，见 kunxcore/src/main.rs），
  内核 `vfs::open` 读取三份文件后经 `cyw_wifi_set_firmware(&'static [u8])` 注入 ——
  **blob 必须保存在静态/不回收内存**（如 `include_bytes!` 全局 static）。
- 方案 B：直接内嵌进 ramdisk/内核只读段（对 500KB blob 会显著增大内核；QEMU 无
  WiFi 不需要）。
- CLM blob 与 NVRAM 由固件下载阶段依次下发（或 NVRAM 在运行时以 iovar 设置），
  时序与地址常量见 §6。

### 2.4 DHCP 客户端是否已有实现？

**原内核没有（IP 为硬编码 QEMU slirp 默认值）。** 本次新增：

- `kunxcore/src/net_stack.rs` 的**极简 DHCP 客户端**（RFC 2131：DISCOVER →
  OFFER → REQUEST → ACK，`dhcp_configure()`），并给 `ip_send` 加了有限广播
  直发路径（0.0.0.0 源 + 全 1 目标 MAC，符合 DHCP 语义）。
- 真实后端（Pi4）连接成功后在 `kunxcore::wifi::finish_connect` 调用线上 DHCP；
  QEMU 模拟后端经 `WifiDevice::lease()` 直接给出与 slirp 一致的静态租约
  （10.0.2.15/24），保证模拟「已连接 → 可上网」不损坏现有 eth0 网络。
- 后续可对虚拟用户网络/真实 AP 均用同一客户端（去掉模拟租约分支即可）。

---

## 3. 组件与文件

| 组件 | 位置 | 内容 |
| --- | --- | --- |
| Wi-Fi HAL | `kunxhal/src/wifi.rs` | `WifiDevice` trait：扫描/关联/EAPOL 中继/状态/租约 |
| QEMU 后端 | `kunxhal/src/plat_qemu.rs` | `SimWifi`：确定性 AP 表 + 真实 AP 侧 4-way + 静态租约 |
| Pi4 后端 | `kunxhal/src/plat_pi4.rs` | `CywWifi`：dwmmc/SDIO/固件下载/WLC 常量（骨架） |
| 内核驱动 | `kunxcore/src/wifi.rs` | `/dev/wifi` 字符设备、事件队列、4-way STA 侧、重试、DHCP 触发 |
| DHCP | `kunxcore/src/net_stack.rs` | RFC 2131 极简客户端 + 广播 TX |
| 用户态守护进程 | `kunxserv/wifid/` | 状态机、配置存储、`/var/run/wifid.pipe` |
| 启动接入 | `kunxinit/` + `Makefile` | 拉起 `/bin/wifid`；`make wifid` / `make test-wifi` |
| 协议常量 | `kunxwireless::devproto` | /dev/wifi 命令/事件/记录 字节布局 |

---

## 4. 状态机

内核（/dev/wifi 语义，`WIFI_STATE_*`）：`Idle → Scanning → Connecting(关联) →
Handshake(4-way) → Connected`；失败重试（`CONNECT_RETRY_MAX=3`），耗尽后
`Disconnected` 并上报 `ERROR` 事件。

wifid 侧（任务要求的语义）：`Idle → Scanning → Associating → Authenticating →
Connected → Disconnected`，由内核事件驱动镜像；`ERROR` 时按目标 SSID/PSK 自动重连。

---

## 5. 配置存储

目录 `/data/wifi/networks/`（持久化 SFS 分区），每网络一个文件：

```
/data/wifi/networks/KunxHome.conf
  ssid=KunxHome
  psk=kunxhome123
  auth=1        # 0=开放 1=WPA2-PSK
  priority=10
```

- v1 口令**明文存储**（任务允许明文或简单加密）；后续可加 XOR/密钥环。
- wifid 启动扫描目录（getdents64）加载全部网络；`AUTO` 选 priority 最高的自动连接。

---

## 6. 构建 / 运行 / 测试

```sh
make wifid          # 单独编译 wifid（kunxserv/wifid）
make                # 全量（内核 + 服务 + rootfs）
make test-wifi      # 无头自检：QEMU -append "wifitest"
                    #   → 扫描 → WPA2-PSK 4-way 连上 KunxHome
                    #   → 错误口令被 AP 拒绝；断言串口 [wifi-selftest] PASS
make run            # QEMU 启动，wifid 自动扫描/尝试已保存网络
make pi4            # Pi4 编译检查（cargo check --features pi4）
```

QEMU 演示内建 AP（SimWifi）：
`KunxHome`(WPA2-PSK, psk=`kunxhome123`)、`KunxGuest`(开放)、`Kunx-IoT-5G`(隐藏
WPA2-PSK, psk=`iot2026!net`)、`KunxOffice`(WPA3，演示「不支持」)。

管道命令示例（在内核 shell 或任意 EL0 写入 `/var/run/wifid.pipe`）：

```
echo "SCAN" > /var/run/wifid.pipe
echo "CONNECT KunxHome kunxhome123 10" > /var/run/wifid.pipe
echo "SAVE Kunx-IoT-5G iot2026!net 20" > /var/run/wifid.pipe
echo "AUTO" > /var/run/wifid.pipe
echo "STATUS" > /var/run/wifid.pipe
echo "FORGET KunxGuest" > /var/run/wifid.pipe
```

---

## 7. Pi4 真机接线清单（TODO，骨架已备）

1. **SDIO 基础**：`dw_init()` 验证（时钟源/分频/使能），`sdio_probe()` 读 CCCR
   制造商/产品（0x08/0x09）确认 CYW43455 响应 —— 这是无需固件的第一个可验证点。
2. **固件下载**（brcmfmac_sdio.c 同款时序）：
   - 功能 1 使能（CCCR IOE）、回板窗口（`SBSDIO_FUNC1_SBADDR = 0x18000000|addr`）。
   - 芯片核心复位（chipcommon `RESET_CTL`），等待 core ready。
   - 向 dongle SDRAM 基址批量写固件（CMD53 块写，SDPCM 未启用前的裸 RAM 上传）
     → CLM blob → NVRAM 配置 → 触发 dongle 启动。
   - `cyw_wifi_set_firmware` 注入的 blob 在此阶段消费；`FW_LOADED=true`。
3. **命令/事件接口**：SDPCM I 消息（WLC_UP/SCAN/JOIN/DISASSOC/GET_RSSI），
   A 消息事件环（SCAN_COMPLETE/ASSOC/PSK_SUP/LINK/DISASSOC）。
4. **扫描结果**：`WLC_SCAN_RESULTS` 拉取 `bss_info` 解析到 `WifiScanResult`。
5. **关联 + 4-way**：`WLC_JOIN`；EAPOL 帧经数据路径（ethertype 0x888E）中继给
   内核 STA 4-way（本仓库已实现），完成后无需 wsec 卸载（自研握手）。
6. **数据通路**：SDPCM 数据帧 ↔ 802.3 帧桥接，接入 `net_stack.poll`/发送路径。
7. **密钥安装**：CCMP PTK/TK 通过 iovar（或 firmware wsec）下发后数据才会加密；
   若走 firmware wsec，Msg2/Msg4 由固件代发（主机只提供 PMK）——两种模式在
  `docs/wireless.md`（内核↔用户协议库的 STA 逻辑说明）中均有注释。

> 现有代码中「TODO 接线点」均以 `接线点：` 注释标明（`kunxhal/src/plat_pi4.rs`）。

---

## 8. 已知限制与下一步

- WPA2-PSK only；WPA3-SAE / 802.1X / WEP 仅识别（`WifiAuth`），关联返回
  `Unsupported`。
- QEMU 模拟后端不模拟射频/漫游（漫游逻辑在 `kunxwireless::wifi_mgmt` 纯函数层）。
- DHCP 客户端无租约续期（`dhcp_configure` 一次性）；后续加 T1/T2。
- Pi4 固件 blob 闭源，仓库不内嵌；`test-wifi`/`make run` 只验证 QEMU 路径。