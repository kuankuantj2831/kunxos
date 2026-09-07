# KunxOS 安全启动链与密钥管理

版本：1.0（v1） · 状态：设计 + 实现（kunxsec / kunxboot / kunxcore 集成）

本文档回答安全启动链的核心问题：BootROM/Bootloader/内核/系统镜像如何逐级验签
（不引入 U-Boot 或 Linux dm-verity，保持自研链），密钥如何分层、烧录、轮换，
以及失败时如何降级/回滚（含 Pi4 与 QEMU 两个目标平台的差异处理）。

配套文档：[`docs/ota.md`](ota.md)（A/B + OTA 更新包 + 增量更新 + 恢复模式）、
[`docs/kunx-package-format.md`](kunx-package-format.md)（应用包，已有实现）。

---

## 1. 安全启动链总览（架构图）

```
┌────────────────────────────────────────────────────────────────────┐
│ 信任根（Root of Trust）                                              │
│   BootROM 固件（硬件） + OTP/eFUSE 熔断值                            │
│   • secure-boot 使能熔丝            （出厂置 1 → 开闭不可逆）         │
│   • BOOT_PK 的 SHA-256 摘要          （授权启动公钥，出厂熔断）       │
│   • 设备唯一密钥 KEY_DEV             （128 位，首启生成/出厂烧录）    │
│   • 反回滚计数器蚂蚁熔丝（只能 1→0） （可选，防降级）                 │
└──────────────┬─────────────────────────────────────────────────────┘
               │ ① BootROM 验签 bootcode（Pi4：Broadcom 官方链；见 §4）
               ▼
  ┌────────────────────────────────────────┐
  │ ② Bootloader  (kunxboot)                │
  │    · 读 MBR → 读 misc BCB（槽位/尝试/回滚）│
  │    · 选槽 A/B → 挂载对应 boot 分区 FAT32  │
  │    · KXSI 验签 KERNEL.IMG / KUNXOS.DTB / │
  │      RAMDISK.IMG（boot_pk，Ed25519）      │
  │    · 任一验签失败 → 尝试另一槽位；全失败   │
  │      → 坠入恢复模式（BCB 置 recovery）     │
  │    · DTB fixup：注入安全属性（§5）并跳转   │
  └──────────────┬─────────────────────────┘
                 │ ③ 跳转（关闭 MMU/缓存，x0=DTB）
                 ▼
  ┌────────────────────────────────────────┐
  │ ③ Kernel (kunxcore)                     │
  │    · 读 DTB kunx,secure-boot 属性        │
  │    · 自校验内嵌 ramdisk（SHA-256 对照）   │
  │    · BCB 标记启动成功（slot_ok+版本）     │
  │    · bootargs 含 recovery → 挂载恢复    │
  │      ramdisk 并启动 /bin/recovery        │
  │    · OTA 服务：下载→验签→写非活动槽→       │
  │      切换 BCB（详见 docs/ota.md）          │
  └──────────────┬─────────────────────────┘
                 │ ④ 系统镜像：ramdisk 为 KXSI 签名镜像
                 ▼
  ┌────────────────────────────────────────┐
  │ ④ Init/系统镜像                          │
  │    · /init = 内核内嵌 ramdisk 内的 ELF   │
  │    · 应用包验签（kunxpkg，KUNXPKG +      │
  │      证书链，已有实现）                   │
  │    · OTA 客户端（otad 用户态 + 内核 ota） │
  └────────────────────────────────────────┘
```

### 信任链语义

| 环节 | 验什么 | 用什么密钥 | 校验目标 |
|------|--------|------------|----------|
| BootROM → bootcode | Broadcom 固件 | RPF 芯片内嵌密钥（Pi4） | 防止篡改 GPU 固件 |
| Bootloader → 内核/DTB/ramdisk | KXSI 镜像 | `boot_pk`（OTP 熔断摘要锚定） | 防止恶意系统替换 |
| 内核 → 内嵌 ramdisk | SHA-256 自检 + 启动链结果 | 内核内嵌 `boot_pk` | 防止编译链被替换 |
| 内核/OTAd → 更新包 | KXUP 包 | `ota_pk`（更新签名密钥） | 防止伪造 OTA |
| kunxpkg → 应用包 | KUNXPKG 包 + 证书链 | `root_anchor` | 防止伪造应用（已有） |

### 密钥分层（离线私钥、设备只持有公钥/摘要）

```
               ┌─────────────────────────────┐
               │  ROOT 主密钥（离线冷存储）    │
               │  签发下一级密钥的"授权证书"   │
               └──────┬──────────────────────┘
                      │ 签发/授权
        ┌─────────────┼──────────────────┐
        ▼             ▼                  ▼
 ┌──────────────┐ ┌──────────────┐ ┌──────────────────┐
 │ boot_pk/sk   │ │ ota_pk/sk    │ │ (可选) 中间证书     │
 │ 签名启动镜像   │ │ 签名更新包    │ │ 签发开发者证书     │
 │ (KXSI)       │ │ (KXUP)       │ │ (已有 kunxpkg)    │
 └──────────────┘ └──────────────┘ └──────────────────┘
  公钥：            公钥：
  · 熔断于 OTP      · 内嵌于内核与 bootloader
  · 内嵌于 bootloader
  · 内嵌于内核（OTA 回验）
```

- **boot_pk**：安全启动链的实质锚点。公钥的 SHA-256 **熔断在 OTP**（出厂），
  bootloader 与内核各自内嵌同一份公钥副本。bootloader 验签时先校验
  `sha256(内嵌公钥) == OTP 熔断值`（防镜像被替换后连内嵌公钥一起换掉），
  再对镜像做 Ed25519 验签。
- **ota_pk**：仅用于验证更新包（KXUP），不能启动系统。即使 ota 私钥失泄，
  攻击者最多能提交"外观合法"的更新包，但 bootloader 仍用 boot_pk 验签每个
  组件镜像 → 双钥分隔把单点失泄限制为"不可用"而非"可植入"。
- **设备唯一密钥（KEY_DEV）**：熔断于 OTP 或首启生成，用于 /data 分区加密
  （现为 XOR demo 密钥，替换为 KEY_DEV 派生密钥）、设备认证与远程锁定。
  丢失不可恢复（OTP 语义）。

---

## 2. 组件镜像格式（KXSI）

当前内核以裸二进制加载（无头部）。本设计新增 **KXSI v1**（KunxOS Signed
Image）容器，`kunxsec/src/kxsi.rs` 实现解析/验签，`tools/kunxsec/kxsi_build.py`
实现打包/签名。**已有磁盘上的 KERNEL.IMG 等文件保持不变**——bootloader 读取
的仍是同名文件，只是内容变成 KXSI 容器（头部 + 载荷 + 签名），并增加
`RECOVRD.IMG`（恢复 ramdisk）。

```
KXSI v1（小端，头 80 字节）：
  @0  magic[8]    = "KXIMGKV1"
  @8  pad u32     = 0
  @12 role u32    = 2=内核 3=DTB 4=ramdisk 5=恢复ramdisk 6=系统镜像
  @16 version u32  = 构建版本号（单调递增，用于反降级比较）
  @20 timestamp u64 = 构建时间（Unix 秒）
  @28 reserved u32 = 0
  @32 payload_off u32 = 80（v1 固定）
  @36 payload_len u32
  @40 payload_sha256[32]
  @72 entry u64    = 内核入口（裸二进制约定 0x40080000；0=按角色取默认）
  @80 payload[...payload_len]
       sig[64]     = Ed25519(kxsi_pk, [0, 80+payload_len))
```

验签流程（bootloader / OTA / kunxverify 共用同一份代码）：

1. 魔法、头内各字段边界（`payload_off+payload_len ≤ len-64`，checked 加法）；
2. `role` 与调用方期望角色匹配；
3. 流式 SHA-256 载荷，与头部 `payload_sha256` 一致；
4. Ed25519：签名覆盖 `[0, 80+payload_len)`，公钥 = 内嵌 `boot_pk`
   （bootloader/内核先验 `sha256(boot_pk)==OTP 熔断值`）。

失败处理：验签失败即视为**不可信镜像**，任何环节都不解密、不分析载荷内容
（防恶意象限构造）。bootloader 打印 `[sec] verify FAIL (slot X)` 后进入回滚
流程（§6）。

---

## 3. MBR 下的 A/B 分区布局（关键问题回答）

现状（`tools/mkdisk.py`）：MBR 3 主分区 boot(64M FAT32) / rootfs(128M SFS) /
data(256M SFS/ENC)，共 448MB，磁盘 512MB，尾部剩约 63MB 未用。

MBR 仅 4 个主分区。本设计**不迁移 GPT**（保持与现有内核 blkdev 分区索引兼容），
充分利用第 4 个主分区 + 磁盘尾部原始区：

```
LBA 0          MBR（分区表 4 项 + 0x55AA）
LBA 2048       ┌─────────────────────────────────────┐
               │  P1 boot_a : FAT32, 64MB, active     │ KERNEL.IMG / KUNXOS.DTB /
               │  （当前槽位，bootloader 优先）        │ RAMDISK.IMG / RECOVRD.IMG
LBA 133120     ├─────────────────────────────────────┤
               │  P2 sys    : SFS, 124MB              │ 系统暂存（OTA 下载缓存、
               │                                     │ 离线全量包、回滚快照）
LBA 387072     ├─────────────────────────────────────┤
               │  P3 data   : SFS/ENC, 256MB          │ /data 持久化（保持 sda3，
               │                                     │ 内核代码无需改分区号）
LBA 911360     ├─────────────────────────────────────┤
               │  P4 boot_b : FAT32, 64MB             │ 非活动槽位（同样三镜像+KXSI）
LBA 1042432    ├─────────────────────────────────────┤
               │  misc 原始区 3MB（非分区，裸扇区）    │
               │   @ +0  BCB 主   （512B，槽位/尝试/  │
               │              成功标志）               │
               │   @ +1  BCB 备份  （512B）           │
               │   @ +2..+9 OTP 模拟区（4KB eFUSE）   │
               │   其余保留（恢复模式标志、未来 TEE）  │
LBA 1048576    └─────────────────────────────────────┘（512MB 磁盘尾）
```

要点：

- **P3 仍是 data**（内核 `/dev/sda3` 逻辑不变）；P1/P4 都是 FAT32，现有内核
  MBR 循环会注册它们并自动挂载 FAT32（P1 → `/mnt/fat32`，P4 → `/mnt/fat32b`）。
- **misc 不是分区**，避免占用主分区名额；由 bootloader 与内核按固定 LBA 直接
  裸扇区读写（两个 BCB 副本 + 代次计数，写主前先写备，崩溃可恢复）。
- 每槽位独立存放完整三镜像（kernel+dtb+ramdisk），槽位间完全隔离；
  `boot_b` 出厂即被工厂镜像签名并填充（可启动的占位槽），OTA 只是**原位覆写**
  非活动槽的文件（新镜像 < 槽位容量上限即可）。
- 若未来需要更多分区（recovery 独立分区 / 更多槽位），迁移 GPT 即可，
  格式/验签逻辑不变。

---

## 4. 关键问题：Pi4 BootROM / QEMU OTP

### 4.1 Pi4 的 BootROM 能否验签我们的 bootloader？

**不可以直接验签。** Pi 4 的启动顺序是：

```
VideoCore BootROM → bootcode.bin → start.elf（GPU 固件）→ 加载 ARM 内核镜像
```

- 出厂默认 **secure boot 关闭**：BootROM 对 bootcode.bin 只校验 Broadcom/RPF
  签名（芯片内嵌密钥），且默认放行已签名旧固件。**我们的 bootloader 是 GPU
  固件之后加载的 ARM 代码**，不在 BootROM 的验签范围内。
- 要获得真正的"BootROM 验签"闭环，需启用树莓派官方 secure-boot 流程：
  通过 OTP 熔丝 `SECURE_BOOT` 相关位 + 向 RPF 申请自定义密钥对，让 BootROM
  只接受**由你可控密钥签名的固件包**（bootcode/start.elf）。这是一次性、
  仅 CM/启用流程可做，且需保 GPU blob。
- **因此自研链的信任起点是"BootROM→固件"之后的 Bootloader**：在 Pi4 上，
  我们的安全链 = `BootROM(信任Broadcom固件) → kunxboot(验签内核/DTB/ramdisk) →
  KunxOS 内核 → 系统镜像`。文档其余部分描述的根信任（OTP 熔断 boot_pk）
  在 Pi4 上由 kunxboot 自身校验（OTP 模拟区 → 真机 OTP 融合地址），
  BootROM 一环属于平台厂商信任，不属于我们能自研的部分。
- 若必须在 Pi4 上提供完整闭环：计划 = 启用 RPF secure boot 把 GPU 固件锚定，
  再叠加本设计的 kunxboot 验签链；QEMU 中我们可模拟完整链（BootROM 环节用
  `-device loader` 固定加载 bootloader 镜像 + 验签流程全走 kunxsec）。

### 4.2 QEMU 如何模拟 eFUSE/OTP 不可变存储？

QEMU virt 无真实 eFUSE。用 **磁盘 misc 原始区 + 软件写一次语义** 模拟：

- OTP 区域 = misc 区 4KB（8 扇区），bootloader/内核按裸扇区读写；
- **写一次语义**：每个字节初值为 0xFF；`otp_write(off, byte)` 仅当目标字节
  仍为 0xFF 时才允许写入（熔断后不可逆），程序化保证与 eFUSE 一致；
- **反回滚计数器**用"熔丝位计数"模拟：64 字节计数器域中 1→0 熔断的位数即
  当前楼层值（只增不减，符合 eFUSE 计数器只能烧的能力）；
- 首启时写入 magic/版本/使能熔丝；`secure-boot` 使能、`boot_pk` 摘要、设备
  唯一密钥均在此区熔断一次。跨重启持久化（磁盘落盘）。
- 内核/工具可 `ota status` 查看 OTP 熔断状态；`tools/kunxsec/otp_dump.py`
  可离线查看镜像中的 OTP 区。

### 4.3 当前内核镜像格式？

已确认：**当前为裸二进制**（QEMU virt 链接于 `0x40080000`，`load_kernel.rs`
直接 `read_file` 复制，无任何头部）。本设计（§2 KXSI）为其增加 80 字节头部 +
64 字节签名；跳转入口由头部 `entry` 字段给出（默认仍 0x40080000）。
DTB 与 ramdisk 同样包装为 KXSI（role=3/4）。

### 4.4 bsdiff 在 Rust 中的实现/绑定方案？

见 [`docs/ota.md` §4](ota.md)（KXDELTA：BSDIFF40 结构自研 + 轻量压缩替代 bzip2，
并给出真实 bsdiff 的 FFI/纯 Rust 移植选项）。

---

## 5. 安全链验签实现要点（kunxboot / kunxcore）

### 5.1 bootloader（kunxboot/src/sec_boot.rs）

```
启动流程（替换 main 中原"找第一个 FAT32"逻辑）：
1. 读 MBR；读 misc 区 BCB 主/备副本 → bcb（无效则按全新 BCB 处理）
2. 模式决策：
   · boot_mode==recovery → 选当前 active 槽位的 boot 卷的恢复镜像
   · 否则 → slot = active；若 try 归零且 active 槽未成功标记
            → slot = 另一槽（自动回滚）
3. 挂载对应 boot 卷（fs_fat::mount_index(slot)）
4. 依次读 KERNEL.IMG / KUNXOS.DTB / RAMDISK.IMG 到约定地址（容量上限内）
5. KXSI 验签（kunxsec::kxsi::verify，boot_pk，先校验 OTP 公钥摘要）
   · 任一失败 → 尝试另一槽；两个槽都失败 → BCB 置 recovery + 错误闪烁
6. DTB fixup：补 bootargs；向 /chosen 注入
   kunx,secure-boot / kunx,boot-slot / kunx,build-version /
   kunx,recovery-ramdisk = <addr size>（恢复模式时才注入 ramdisk 地址）
7. jump_to_kernel（x0=DTB 地址）
```

### 5.2 内核（kunxcore/src/ota.rs）

- 启动成功后 `bcnote_boot_success()`：`slot_ok[active]=1`、`tries=0`、
  `build_version=max(build_version, 镜像版本)`、写主/备 BCB；
- 读取 DTB 安全属性并打印启动链日志（`[sec] slot=A secure=1 build=42`）；
- bootargs 含 `recovery` 或 BCB 模式为 recovery → 挂载恢复 ramdisk 到
  `/mnt/recovery` 并启动 `/bin/recovery`（详见 ota.md §6）；
- OTA 安装/反降级/槽位切换（详见 ota.md §5）；
- shell 命令 `ota status|check|install|activate|recovery|bump-floor`。

---

## 6. 失败处理策略

| 失败场景 | 检测点 | 动作 | 恢复 |
|---|---|---|---|
| 槽 A 镜像验签失败 | bootloader | 记录，尝试槽 B | B 启动成功后 BCB 标记 |
| 槽 A/B 都验签失败 | bootloader | 置恢复模式，停留提示 | 手动刷机/工厂恢复 |
| 新槽启动连续崩溃 | bootloader 尝试计数归零 | 自动切回旧槽 | 旧槽启动成功后复位 |
| 内核 panic（启动早期） | 内核 panic 前写入的 BCB 无成功标记 | 下次重启尝试计数-1 | 同"连续崩溃" |
| OTA 包验签失败 | 内核 ota install | 拒绝，不清 BCB | 系统保持当前槽 |
| 降级包（version≤floor） | 内核 ota install | 拒绝（反降级） | 系统保持当前槽 |
| misc BCB 撕裂 | bootloader 读主+备 + CRC | 取有效副本/回退全新 | 全新槽 A 启动 |
| DTB/TEE 属性缺失 | 内核 | 记录告警（不阻断旧镜像） | 保持启动 |

---

## 7. 密钥生命周期

### 7.1 出厂烧录（Factory Provisioning）

```
工厂（离线/受控环境）：
1. 生成 boot 密钥对；把 boot_pk 摘要烧入 OTP（熔丝）
2. 生成设备唯一密钥 KEY_DEV → 烧入 OTP（或首启经 virtio-rng 生成）
3. 使能 secure-boot 熔丝（在验证全部镜像可启动之后，一次性）
4. 用 boot_sk 签发出厂 KERNEL/DTB/RAMDISK/RECOVRD（KXSI），写入 boot_a；
   同一套镜像写入 boot_b 占位
5. 生成第一版 KXUP 全量包存入 sys 分区 / 镜像仓库
6. 记录：批次、熔断日期、密钥指纹（SHA-256 摘要，不存私钥）
```

烧录顺序很重要：**先签镜像、再熔断 boot_pk 摘要、最后置 secure-boot 使能**。
一旦使能熔丝烧断，未授权镜像将永远无法启动。

### 7.2 密钥轮换

boot_pk 是启动链根，轮换需"信任转移"（旧根仍信任一段时间）：

- **OTA 密钥（ota_pk）轮换**：经 OTA 包本身下发新公钥（包内声明
  `ota_key_update`，新公钥由旧 ota_sk 签名 + boot_pk 背书），下一版起生效；
  私钥失泄同理即刻轮换。
- **boot_pk 轮换**：仅能由 `boot_pk` 签发的"根更新镜像"（KXSI role=7，
  包含新公钥 + 新镜像）在 bootloader 校验后**重新烧写 OTP 熔断值所在位置**
  ——但 OTP 是写一次，所以设计上允许 **ROT 更新槽**（OTP 预烧一个
  `boot_pk_alt` 摘要，正常情况 0xFF 未用；轮换时烧入 alt 摘要，bootloader
  优先以 alt 校验）。提供两代窗口；第三次轮换需硬件回流。
- 文档与工具：`tools/kunxsec/rotate_sim.py` 演示 alt 槽轮换流程（QEMU 模拟）。

### 7.3 私钥失泄处理（Compromise Response）

| 阶段 | 影响 | 处置 |
|---|---|---|
| ota_sk 失泄 | 可伪造更新包 | 立即轮换 ota 密钥并发布强制更新；boot_pk 不受影响 |
| boot_sk 失泄 | 可伪造可启动镜像 | 轮换 boot 密钥（alt 槽）；已部署设备若 secure-boot
  已熔断且无 alt 预留 → 需物理回流/换机 |
| ROOT 主密钥失泄 | 影响全部签发链 | 吊销全部下级证书（kunxpkg 侧已有证书链基础）；
  启动链密钥独立于 ROOT，仅受影响的是应用证书 |

应急工具：`tools/kunxsec/compromise_check.py` 校验设备端 boot_pk 摘要与
OTP 熔断值一致性，输出受信任镜像列表。

---

## 8. 实现状态对照（本文档对应代码）

| 组件 | 代码 |
|---|---|
| KXSI 格式 + 验签 | `kunxsec/src/kxsi.rs`（`kunxsec::kxsi::verify`） |
| BCB A/B 元数据 | `kunxsec/src/bcb.rs` |
| OTP/eFUSE 模拟 | `kunxsec/src/otp.rs` |
| OTA 更新包 | `kunxsec/src/kxup.rs` |
| 增量补丁应用 | `kunxsec/src/bspatch.rs`（KXDELTA） |
| 主机侧打包/签名/验签 | `tools/kunxsec/*.py`（复用 `tools/kunxrepo/ed25519.py`） |
| bootloader 验签/槽位 | `kunxboot/src/sec_boot.rs` |
| 内核 OTA/BCB/恢复 | `kunxcore/src/ota.rs` + shell `ota` 命令 |
| 恢复模式 | 恢复 ramdisk（`RECOVRD.IMG`）+ `apps/recovery` |
| 磁盘布局 | `tools/mkdisk.py`（boot_a/sys/data/boot_b + misc） |