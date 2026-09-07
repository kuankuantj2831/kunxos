# KunxOS OTA 更新与恢复模式

版本：1.0（v1） · 状态：设计 + 实现（kunxsec / kunxcore / apps/recovery）

配套文档：[`docs/secure-boot.md`](secure-boot.md)（安全启动链、密钥、A/B 布局）。

---

## 1. 总览

OTA 目标：后台下载、验签、写入非活动槽位、原子切换、失败自动回滚；
增量更新减少流量；反降级；恢复模式兜底。

```
用户态 otad / 内核 shell ota
   │ 1. 从仓库下载 KXUP 包（全量或增量），存 /data/ota/
   ▼
内核 ota install <file>
   │ 2. 验 KXUP 签名（ota_pk）
   │ 3. 反降级检查（new_version > max(bcb.build_version, OTP floor)）
   │ 4. 组件处理：
   │     全量组件 → 直接验 KXSI(boot_pk) 后写入非活动槽
   │     增量组件 → 读非活动槽当前 blob → KXDELTA 应用 → 验签 → 写回
   │ 5. 写后回读自验（KXSI + SHA-256 对照 manifest）
   │ 6. 更新 BCB：active=非活动槽、tries_total=3、tries=3、ota_in_progress=1
   ▼
重启 → bootloader 读 BCB → 选新槽 → KXSI 验签 → 启动
   ├─ 成功 → 内核 mark_boot_success（slot_ok=1, tries=0, build_version 提升）
   └─ 连续失败（tries 归零）→ bootloader 自动回滚到旧槽
```

## 2. 更新包格式（KXUP v1，kunxsec/src/kxup.rs）

```
KXUP v1（小端）：
  @0  magic[8]       = "KXUPDv10"
  @8  format u32     = 1
  @12 target_role u32 = 6（系统镜像包）
  @16 target_version u32 （新构建号，单调递增）
  @20 base_version u32   （0=全量；非 0=从该版本增量）
  @24 flags u32          （bit0=含增量组件）
  @28 manifest_len u32
  @32 components_count u32
  @36 reserved u32 = 0
  @40 components 区（每个 = 16 字节头 + 数据）：
       { role u32; data_len u32; delta_len u32; flags u32 }
       · flags bit0 = 增量（data 区存 KXDELTA 补丁）
       · flags bit1 = 全量（data 区为完整 KXSI 镜像）
       全量组件 data 区 = 完整 KXSI blob；
       增量组件 data 区 = KXDELTA 补丁（生成最终 KXSI blob）
  【后接】 manifest[...manifest_len]（UTF-8）
  【最后】 sig[64] = Ed25519(ota_pk, [0, len-64))
```

验签顺序：
1. 结构边界（checked 算术，拒绝重叠/越界）；
2. 整包 Ed25519（`ota_pk`）；
3. manifest 与头部版本一致（`target_version`）；
4. 每个**全量**组件在安装前再单独 KXSI 验签（`boot_pk`）——双钥校验。

manifest 文本字段（KXMAN v1）：

```
name=kunxos
target_version=42
base_version=0
description=build 42
sha256_full=<64 hex>   # 全部最终组件串联的 SHA-256，安装后自验
```

## 3. A/B 启动控制块（BCB，kunxsec/src/bcb.rs）

misc 区两个 512 字节副本（主+备），CRC32 + 代次计数双副本可恢复。

```
@0  magic[8] = "KXBCBv10"
@8  version u32 = 1
@12 active_slot u8      0=A, 1=B
@13 tries_remaining u8  当前槽剩余尝试次数
@14 tries_total u8      切换/OTA 写入的初始尝试次数（默认 3）
@15 slot_ok[2] u8       各槽是否曾成功启动（0/1）
@17 boot_mode u8        0=normal 1=recovery
@18 ota_in_progress u8  1=OTA 已写入待重启切换
@19 reserved u8 = 0
@20 build_version u32   当前已确认的构建版本（反降级下限）
@24 antirollback_floor u32 OTP 熔断下限快照
@28 gen u32             代次计数（主/备交替写，重启取 gen 大者）
@32 reserved[472]
@504 crc32 u32          crc32([0..504))
```

读写语义：
- bootloader 每次启动仅**读** BCB（不写），失败时写回滚切换；
- 内核 `mark_boot_success()`：`slot_ok[active]=1`、`tries_remaining=0`、
  `build_version=max(build_version, 镜像版本)`、`ota_in_progress=0`；
- 连续崩溃回滚：新槽在 tries 耗尽前始终无成功标记，bootloader 在
  `tries_remaining==0 && slot_ok[active]==0` 时自动切另一槽。

## 4. 增量更新（KXDELTA，kunxsec/src/bspatch.rs）

### 4.1 方案对比（回答"bsdiff 的 Rust 实现/绑定"）

真实 bsdiff 补丁用 bzip2 压缩。三选一：

| 方案 | 说明 | 取舍 |
|---|---|---|
| A. FFI 绑定 `bspatch` | 链接 Colin Percival 的 C 代码，依赖 pthread+bzip2 | 引入重依赖，裸机难 |
| B. 纯 Rust 移植 | 现有 bspatch crate 为 BSDIFF40 纯 Rust，但仍需 bzip2 解码器 | 需带 bzip2 解码 |
| C. **KXDELTA（本项目）** | 保留 BSDIFF40 控制流结构（diff/extra/seek 三元组语义），压缩层用自研 KXRL1（LZSS 变体）替代 bzip2 | 简单、可审计、零依赖；压缩率略低 |

本项目选 **C**：结构与 BSDIFF40 同构，不同处仅压缩格式。日后可把 KXRL1
替换为 bzip2 而不改控制流代码。

### 4.2 KXDELTA v1 格式

```
@0  magic[8] = "KXDELTA1"
@8  format u32 = 1
@12 new_len u32     （产出镜像大小）
@16 ctrl_len u32    （控制块字节数）
@20 diff_len u32    （diff 块解码后字节数）
@24 extra_len u32   （extra 块解码后字节数）
@28 flags u32       （bit0=diff 块 KXRL1 压缩, bit1=extra 块 KXRL1 压缩）
@32 reserved u32 = 0
@36 ctrl[...]       每 24 字节一个三元组（LE i64）：diff_len / extra_len / seek
     diff[...]      （全量或 KXRL1 压缩）
     extra[...]
```

应用算法（bsdiff 经典流程，`bspatch::apply_patch`）：

```
oldpos=0; newpos=0
循环读三元组 (d,e,s)：
  对 d 个字节：out[newpos+i] = old[oldpos+i] + diff[i]   （校验 oldbuf 边界）
  再直拷 e 个 extra 字节
  newpos += d+e; oldpos += d+s
结束时 out 长度必须 == new_len
```

KXRL1 解码（`bspatch::inflate_kxrl1`）：字节流
`0` + 7 位字面量 / `1` + 4 位长度(4..19) + 12 位距离(1..4096)，约 30 行解码器、无外部依赖。

### 4.3 主机生成工具

`tools/kunxsec/bsdiff_gen.py`：对两代镜像做贪心匹配（构造 diff/extra/seek
三元组）并 KXRL1 压缩，产出 KXDELTA 补丁。

## 5. 内核 OTA 安装（kunxcore/src/ota.rs）

```
pub fn ota_install(path: &str) -> i32
  1. 读 KXUP 文件到缓冲区（大小上限校验）
  2. kxup::parse_and_verify(pkg, OTA_PK, BOOT_PK)
  3. 反降级：target_version > max(bcb.build_version, otp_floor, 当前镜像版本)
  4. 确定非活动槽：active==A → 写 P4(boot_b)；active==B → 写 P1(boot_a)
  5. 写入：全量组件 → 已验 KXSI 直接写；增量组件 → 读非活动槽当前 blob →
     KXDELTA 应用 → KXSI 验签 → 写
     写后回读自验（裸扇区读回 + KXSI + SHA-256 对照 manifest.sha256_full）
  6. 更新 BCB（主+备）：active=非活动槽, tries_total=3, tries_remaining=3,
     ota_in_progress=1, gen+=1
  7. 提示重启切换
```

### 5.1 写入非活动槽的实现

- P1/P4 都是 FAT32，内核 MBR 循环会注册并挂载（P4 → `/mnt/fat32b`）。
- P2（sys 暂存分区）当前**不挂载**：重复挂载 SFS 会干扰 VFS 解析状态；
  OTA 包暂存于 `/data/ota/`（/data 为既有持久挂载）。sys 分区保留为
  离线刷机包 / 快照的存放地（未来实现）。
- OTA 用 VFS 写文件：`vfs::open("/mnt/fat32b/KERNEL.IMG", WRITE)` →
  truncate → write → close → fsync。内核 FAT32 支持写
  （kunxcore/src/fat32.rs，已有 cp/写路径）。
- 写后自验：经 blkdev 裸读回刚写的镜像扇区（不经页缓存），再做 KXSI 验签，
  防"写脏后误判成功"。

### 5.2 失败与回滚矩阵

| 阶段失败 | 结果 |
|---|---|
| 下载/签名失败 | 不写任何槽；BCB 不变 |
| 非活动槽写入中途断电 | 槽位损坏；BCB 仍指向旧槽 → 旧槽正常启动 |
| 写入完成但回读自验失败 | 丢弃/重写；BCB 不变 |
| 切换后新槽连续崩溃（tries 耗尽） | bootloader 自动回滚旧槽 |
| 新槽成功启动 | 内核清零 tries，旧槽内容保留为回滚点 |

## 6. 恢复模式（Recovery）

### 6.1 触发

- bootloader：两个槽都验签失败 → 写 BCB `boot_mode=recovery` 并停留提示；
- 用户/OTA：`ota recovery`（设置 recovery 后重启）；
- 内核启动参数含 `recovery`（bootloader 注入）。

### 6.2 组成与流程

- **恢复 ramdisk** `RECOVRD.IMG`（KXSI role=5，随 boot_a 存放），bootloader
  验签后加载到固定地址，DTB 注入 `kunx,recovery-ramdisk=<addr size>`；
- 内核检测 recovery → 把该内存区包装成 ramdisk 块设备挂载到
  `/mnt/recovery`，启动 `/bin/recovery`（用户态串口菜单，`apps/recovery`）；
- 恢复菜单（仅串口）：

```
1. 应用 OTA 包（/data/ota/*.kxup）
2. 清除 /data（重新 mkfs，回出厂）
3. 切到另一槽
4. 重启
5. 显示 OTA/BCB/OTP 状态
```

- 恢复模式不读 /data 敏感内容；清除数据前要求确认。

## 7. 构建与测试

- `make disk`：mkdisk.py 生成新布局 + 签名镜像（demo boot 密钥）；
- `make boot`：QEMU 经 bootloader 走完整安全链（打印 `[sec]` 日志）；
- 测试用例（kunxsec 单元测试 + tools/kunxsec/*.py 主机验证）：
  1. KXSI：篡改任一字节失败、role 不匹配失败、正确镜像通过；
  2. BCB：主/备副本代次恢复、CRC 损坏回退、tries 递减与回滚决策；
  3. KXUP：整包验签、反降级拒绝、增量组件应用后 KXSI 生效；
  4. KXDELTA：构造两代镜像生成补丁 → 应用 → 哈希一致；
  5. OTP：写一次语义、熔丝计数器只增、secure-boot 使能不可逆；
  6. 端到端（QEMU）：`ota status`、`ota check`、失败注入（篡改 KERNEL.IMG）
     验证 bootloader 回滚。

## 8. 目录对照

| 组件 | 路径 |
|---|---|
| BCB / KXSI / KXUP / KXDELTA / OTP | kunxsec/src/ |
| 主机打包/签名 | tools/kunxsec/*.py |
| bootloader 槽位与验签 | kunxboot/src/sec_boot.rs |
| 内核 OTA 服务 | kunxcore/src/ota.rs |
| 恢复菜单 | apps/recovery |
| 磁盘布局 | tools/mkdisk.py |