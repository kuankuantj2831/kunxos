# KunxOS `.kunx` 应用包格式规范

版本：1.0（format_version = 1） · 状态：已实现（kunxpkg / tools/kunxrepo）

`.kunx` 是 KunxOS 原生应用包格式（类比 APK/IPA）：一个自包含的自定义归档，
由 **头部 + manifest.json + 载荷 + META-INF 证书链 + Ed25519 签名块** 组成。
与旧版 `.pkg`（NOVAPKG1，FNV-1a 完整性哈希，可伪造）不兼容；kunxpkg 对旧格式
仅做兼容读取并明确告警。

## 1. 总体布局

```text
offset  0:  magic[8]        = "KUNXPKG1"
offset  8:  format_version  u32 = 1
offset 12:  flags           u32（bit0 = signed；其余保留 = 0）
offset 16:  manifest_off    u32 = 48
offset 20:  manifest_len    u32
offset 24:  payload_off     u32
offset 28:  payload_len     u32
offset 32:  meta_off        u32   （META-INF/ 内容：证书链，leaf 在前）
offset 36:  meta_len        u32
offset 40:  sig_off         u32
offset 44:  sig_len         u32   （64 = Ed25519；signed 标志=0 时为 0）
offset 48:  manifest.json
            载荷条目流（见 §3）
            META-INF 证书（见 §4）
            Ed25519 签名（64 字节，覆盖 [0, sig_off) 全部字节）
```

所有多字节整数为**小端**。头部固定 48 字节；各区域顺序严格为
manifest → payload → meta → signature，且边界必须精确衔接（解析器拒绝任何
重叠/越界/乱序布局，防恶意包）。

## 2. manifest.json

UTF-8 JSON，字段：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✅ | 包名：`[A-Za-z0-9._-]`，≤ 28，不含 `/`（防路径穿越） |
| `version` | string | ✅ | 语义版本 `X.Y.Z[-prerelease][+build]` |
| `api_level` | number | ✅ | 要求的系统 API 级别（`≤` 系统 `API_LEVEL` 才可安装） |
| `main` | string | ✅ | 入口 ELF 在载荷中的路径，如 `"bin/calculator"` |
| `description` | string | | 描述 |
| `author` | string | | 作者 |
| `permissions` | string[] | | 权限声明（见 `docs/kunx-package-manager.md` §权限模型） |
| `dependencies` | object | | `{ "libc": ">=1.0.0", ... }`，值为版本约束 |
| `provides` | string[] | | 提供的虚拟名（冲突检测用） |
| `conflicts` | string[] | | 冲突的包/虚拟名（已安装则拒绝安装） |

示例：

```json
{
  "name": "calculator",
  "version": "1.0.0",
  "api_level": 1,
  "description": "KunxOS 计算器",
  "author": "KunxOS Team",
  "main": "bin/calculator",
  "permissions": ["INTERNET", "LOCATION"],
  "dependencies": {"libc": ">=1.0.0"},
  "provides": ["calc"],
  "conflicts": []
}
```

## 3. 载荷条目流

```text
每条：path_len u32 | path[path_len] | data_len u32 | data[data_len]
```

- `path`：应用相对路径，白名单字符 `[A-Za-z0-9._/-]`，拒绝空、绝对路径、
  `..` 段、`.` 段、双斜杠、NUL（`format::valid_path`）。
- 前缀约定（安装器映射）：
  - `bin/<name>` → `/bin/<name>`（入口 ELF）
  - `res/<resname>` → `/res/<name>/<resname>`（应用资源）
  - `lib/<libname>` → `/lib/<libname>`（共享库，供依赖解析/未来动态链接）
  - `data/<rest>` → `/data/app/<name>/<rest>`（沙箱预置数据）
- 条目数上限 64、路径 ≤ 128 字节（防恶意包耗尽解析）。

## 4. META-INF 证书（KUNXCERT）

开发者证书，紧凑二进制（非 X.509），由 KunxOS 自建 CA 签发：

```text
offset 0:  magic[8]        = "KUNXCERT"
offset 8:  version         u32 = 1
offset 12: key_usage       u8（bit0=包签名、bit1=索引签名、bit2=签发下级证书）
offset 13: subject_len     u16
offset 15: valid_from      u64（Unix 秒）
offset 23: valid_to        u64
offset 31: pubkey[32]      Ed25519 公钥
offset 63: subject[subject_len]（UTF-8，CN 等）
           parent_sig[64]  Ed25519 签名（父密钥对 [0, parent_sig) 签名）
```

证书链模型：

```text
系统根锚（内嵌于 kunxpkg，见 §6）→ [中间证书…] → 开发者证书（leaf）
```

- META-INF 内证书 **leaf 在前** 连续存放；链长 1..3（根 → 中间 → 开发者）。
- 验证顺序：从链顶（最接近根的证书）向 leaf 逐级验证 `parent_sig`；
  链顶与中间证书须具备 `KU_CERT`，leaf 须具备 `KU_PACKAGE`；
  全部证书须在有效期内（系统时间不可用时跳过时间校验并告警）。
- 包签名用 **leaf 公钥** 验证。

## 5. 签名

- 签名算法：Ed25519（RFC 8032），64 字节。
- 签名覆盖：`[0, sig_off)`（头部 + manifest + 载荷 + 证书链）。
- 完整验证链：**根锚验证证书链 → leaf 公钥验证包签名**。

## 6. 信任锚与密钥分发

| 锚 | 位置 | 用途 |
|---|---|---|
| 根 CA 公钥 | 内嵌 `kunxpkg/src/trust.rs` 的 `ROOT_ANCHOR` | 验证开发者证书链 |
| 仓库索引公钥 | 内嵌 `REPO_ANCHOR` | 验证软件源索引/增量补丁签名 |

- **根私钥不进入设备镜像**，离线保管（demo 密钥见 `tools/kunxrepo/keys/`，
  仅开发/测试用）。
- 开发者证书随 `.kunx` 包分发（META-INF 内），因此**密钥分发 = 包分发**：
  设备只信任「由根锚签发的证书」及其签名的包。

## 7. 主机侧工具

| 工具 | 作用 |
|---|---|
| `tools/kunxrepo/genkeys.py` | 生成根/仓库/开发者密钥，签发开发者证书 |
| `tools/kunxrepo/mkkunx.py` | 按本规范构建并签名 `.kunx`（纯 Python Ed25519） |
| `tools/kunxrepo/kunxrepo.py` | 生成签名索引 `Packages`/`Packages.sig`/`Packages.meta` 与增量补丁，HTTP 分发 |
| `tools/kunxverify` | 主机侧验签 CLI：**复用设备端同一份** Ed25519/格式/证书代码（`#[path]` 单源） |
| `tools/kunxrepo/build_demo_repo.py` | 用真实应用 ELF 构建 demo 仓库（rootfs 集成） |

## 8. 索引 / 增量补丁格式

- `Packages`：Debian Packages 风格文本，扩展字段
  `ApiLevel` / `Permissions` / `Depends`（`name constraint`）/
  `Provides` / `Conflicts` / `Filename` / `Size` / `KunxHash`（SHA-512 前 16 字节 hex）/
  `SignedBy`；每包仅保留**最新版本**记录。
- `Packages.sig`：仓库密钥对 `Packages` 原始字节的 Ed25519 签名（64 字节）。
- `Packages.meta`：`Revision: N` / `Count: M`。
- `delta-<base>-<target>`（`KUNXDELTA1`）：记录级补丁，格式：

  ```text
  KUNXDELTA1
  Base: <base-rev>
  Target: <target-rev>

  A <Package: name>        # 新增/替换记录（到空行）
  <record 行…>

  D <Package: name>        # 删除记录
  ```

  补丁同样有 `.sig`（仓库密钥签名），客户端验签后才应用。
