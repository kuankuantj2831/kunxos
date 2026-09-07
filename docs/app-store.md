# 应用商店与 R2 软件源（apps/store）

KunxOS 应用商店连接 Cloudflare R2（S3 兼容 API）作为软件源：
下载 `Packages.json` 索引 → 分类/搜索/详情/安装按钮 → 下载 `.kunx` 包 →
Ed25519 验签（复用 kunxsec/kunxpkg，见提示词 #31）→ 调 `/bin/pkg install-file` 安装。

## 架构

```
apps/store/src/
├── main.rs   商店 GUI（列表/详情/设置）、配置读写、索引同步、下载+验签+安装编排
├── r2.rs     R2/S3 客户端：AWS SigV4（HMAC-SHA256 手写）+ s3_get/s3_download
└── index.rs  Packages.json 解析（复用 kunxpkg 极简 JSON 解析器）与只读视图
```

复用（`#[path]` 引入，不复制代码）：
- `kunxpkg/src/{json,format,trust,perms,cert,review,semver,ed25519,sha512,hex}.rs`
- `kunxsec/src/sha256.rs`（SigV4 哈希）
- `shared/{ttf_text,nav,error_ui,cursor_overlay}.rs`

## Packages.json（R2 桶根）

```json
{
  "revision": 4,
  "updated": 1755300000,
  "packages": {
    "notes": { "version": "1.3.0", "description": "轻量笔记",
               "author": "KunxOS Team", "category": "工具",
               "filename": "packages/notes-1.3.0.kunx",
               "size": 24576, "min_api": 1,
               "permissions": ["STORAGE"] },
    "weather": { "version": "1.2.0", "category": "工具",
                 "filename": "packages/weather-1.2.0.kunx", "size": 28672 }
  }
}
```

- 包名 = 对象键（省 name 字段）。`permissions` 为字符串数组。
- 可选 `Packages.json.sig`（64 字节 Ed25519，REPO_ANCHOR）：存在则验签，失败拒绝；
  缺失/拉取失败放行并记录（索引仅用于展示，包安装时仍有独立验签）。
- 可兼容 Debian 风格文本索引：R2 未配置/拉取失败时回退 `/var/lib/pkg/available`
  （旧本地目录），安装走 `pkg install <name>`。

## R2 客户端（r2.rs）

- AWS Signature Version 4：canonical request → string-to-sign → 派生签名密钥
  （kDate/kRegion/kService/kSigning）→ `Authorization` 头。
- payload 哈希 = SHA-256("")；UTC 时间戳由墙钟（SYS_GETTIMEOFDAY）换算；
  端点 path-style（`/<bucket>/<key>`）；GET Object 无请求体。
- 网络层与天气/云应用同一套：SYS_SOCKET/CONNECT/SENDTO/RECVFROM/DNS_SHUTDOWN。
- 错误码：`R2_ERR_*`（-30..-41），403/404 单独区分（凭证错误/对象不存在）。
- `https://` 端点：内核无 TLS 栈 → **明确拒绝**（R2_ERR_TLS），不静默降级；
  待内核 TLS 落地后同一签名代码路径直接可用。演示/联调可用本地
  S3/R2 兼容网关（`http://host:port`）。

## 索引同步与缓存

- 缓存 `/data/store/cache/index.bin`：`KXST1 <unix秒>\n` + JSON body。
- 加载策略：新鲜缓存（TTL 12h）→ R2 拉取（网络错误退避 1s/2s 重试至多 3 次，
  4xx/认证立即失败）→ 过期缓存回退 → 本地目录回退。
- 更新检查：`pkg_install_state` 读 `/var/lib/pkg/installed/<name>` 的 `version=`
  字段，与索引版本 semver 比较 → 安装 / 已装 / 更新。

## 下载与安装

1. 下载对象键：索引 `filename`，否则 `packages/<name>-<version>.kunx`。
2. `s3_download` 流式写入 `/data/store/cache/<name>-<version>.kunx`（重试 3 次）。
3. 读回内存 → `review::review`（证书链对 ROOT_ANCHOR + 包签名对 leaf 公钥 +
   API 级别 + 危险权限计数）。
4. 验签失败：删除缓存文件、提示错误；成功 → `/bin/pkg install-file <path> --yes`。

断点续传预留：`r2::s3_head`（Content-Length/ETag）+ Range 头已在模块注释标注，
下载完成后文件保留为缓存。

## 配置（/data/store/config）

`key=value` 每行一条：`endpoint` / `bucket` / `region` / `access_key` / `secret_key`。
- 设置页（商店内）可编辑全部字段并保存；「测试连接」用 GET Packages.json 验证；
  「同步索引」即刻重拉。
- 多软件源：本次为单源（结构即一组字段）；文件格式天然可扩展为多节/多行，
  预留 sources 目录方案（后续可切换源 0/1/...）。

## 设计决策（关键问题）

1. **R2 凭证配置**：商店内设置页输入（设置 → 保存 → 测试连接），存 `/data/store/config`；
   OOBE 不引入额外步骤（凭证属于商店域，避免 OOBE 膨胀）。首启未配置时商店
   回退本地目录并给出引导提示。
2. **多软件源**：本次单 R2 源；模型 `R2Config` 与索引缓存目录（`cache/index/<n>/`）
   已为多源预留，UI 仅展示源 0。
3. **大包下载后台化**：本版本前台同步（与天气/云应用一致的阻塞 HTTP 模式），
   进度写入状态行；Home 后台化由 lifd SIGSTOP/SIGCONT + launcher 帧快照恢复。
   lifd 后台任务白名单已预留 `BGT_DOWNLOAD=3`，真正守护进程接管的后台下载与
   断点续传（s3_head + Range）为后续迭代。
4. **重试策略**：网络层错误（DNS/TCP/发送/接收）退避重试至多 3 次（1s/2s）；
   403/404/HTTP 语义错误立即失败（避免无意义重试与错误凭证打爆日志）；索引
   失败回退缓存/本地目录，包下载失败回退删除半成品。