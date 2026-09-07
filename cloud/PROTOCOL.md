# KunxOS 云服务 API 契约（v1）

本文档是**唯一权威**的客户端 ↔ 服务端协议。KunxOS 客户机（`apps/cloud`）、
明文 HTTP 网关（`cloud/gateway/gateway.py`）与 Cloudflare Worker
（`cloud/worker/`）三端必须严格遵循本契约。

## 约束

- 传输：**明文 HTTP/1.1**（KunxOS 客户机内核 TCP 栈无 TLS）。
- 编码：UTF-8；请求/响应体均为纯文本行（`\n` 分隔）。
- 认证：每个设备 API 请求携带 `u`（账号/用户名）、`t`（令牌）、`d`（设备 ID）。
- 路径：设备 API 一律 `/api/*`；管理 API 带 `key`（管理密钥）查询参数。

## 通用响应格式

- 成功：第一行 `OK`（可带 `:<附加信息>`）。
- 失败：第一行 `ERR:<代码>`，代码为小写 ASCII，如 `bad_request`、
  `bad_auth`、`not_found`、`exists`、`internal`、`denied`。
- **HTTP 状态码**：设备端只读首行 `OK`/`ERR:<代码>`（不依赖状态码）；
  网关/Worker 同时把业务错误映射为真实状态码，便于 Web 与管理端区分：

  | `ERR:<代码>` | HTTP 状态码 | 含义 |
  | --- | --- | --- |
  | `bad_request` | 400 | 请求体缺失/格式错误 |
  | `bad_auth` | 401 | 账号/令牌/设备不匹配 |
  | `not_found` | 404 | 资源不存在 / 路径未匹配 |
  | `exists` | 409 | 设备已被其他账号注册 |
  | `internal` | 500 | 服务端内部错误（**不携带异常详情**） |
  | `denied`（管理 API JSON） | 200 | 管理密钥错误 |

- **内部错误不泄露细节**：`ERR:internal` 一律不带异常文本；服务端把详情
  写入自身日志（gateway 的 stderr / Worker 的 console.error），
  设备端只得到脱敏的 `ERR:internal`。
- **结构化 detail（第二行，可选）**：错误响应在 `ERR:<code>` 之后追加一行
  JSON（旧设备只读首行，完全兼容；web/管理端可据此区分错误类型）：

  ```text
  ERR:bad_auth
  {"code":"bad_auth","message":"账号或令牌无效","retryable":false,"http_status":401}
  ```

  `message` 为脱敏中文提示；`retryable` 建议设备据此决定是否自动重试
  （目前仅 `internal` 为 true）；`http_status` 与真实状态码一致。

## 1. 设备注册 `POST /api/register`

请求体（application/x-www-form-urlencoded）：

```
u=<账号>&t=<令牌>&d=<设备ID>&n=<设备名>
```

- 已存在同 `d` 设备：返回 `ERR:exists`（客户端可视为成功）。
- 首次注册：创建设备记录（`last_seen` 置当前时间）。
- 响应：`OK` 或 `ERR:<代码>`。

## 2. 数据备份（整快照替换）`POST /api/backup`

请求体为多行文本，前 4 行为头（头行值均做 urlencoded，与其它端点一致），随后 `n` 行文件行：

```
u=<账号 urlencoded>
t=<令牌 urlencoded>
d=<设备ID urlencoded>
n=<文件数>
F|<base64(路径)>|<base64(数据)>
F|<base64(路径)>|<base64(数据)>
...
```

- base64 为 RFC 4648 标准编码（含 `+/=`，无换行）。
- 服务端**整表替换**该设备的所有备份文件（先删旧再写新）。
- 响应：`OK:<文件数>` 或 `ERR:<代码>`。

## 3. 数据恢复 `GET /api/restore?u=&t=&d=`

响应：

```
OK:<文件数>
F|<base64(路径)>|<base64(数据)>
...
```

无备份时：`OK:0`（无文件行）。

## 4. 心跳 + 命令轮询 `POST /api/poll`

请求体（urlencoded）：`u=&t=&d=&ip=<当前IP>`

- 服务端更新设备 `last_seen` 与 `ip`（供「查找设备」使用）。
- 响应：

```
OK
C|<命令ID>|<类型>|<payload urlencoded>
C|<命令ID>|<类型>|<payload urlencoded>
...
```

- `类型` ∈ `find` | `lock` | `wipe`；无待处理命令时只有 `OK` 一行。
- 命令一旦下发即进入 `pending`；客户端执行后必须 `ack`。

## 5. 命令确认 `POST /api/ack`

请求体（urlencoded）：`u=&t=&d=&c=<命令ID>&r=<结果>`

- `r` 为执行结果文本（如 `done`、`ip=10.0.2.15`、`fail:xxx`）。
- 服务端将命令标记为 `done` 并记录结果。
- 响应：`OK` 或 `ERR:<代码>`。

## 6. 管理 API（JSON，供 Web 后台与 curl）

所有管理请求带 `key=<管理密钥>`（查询参数或请求体字段）。

### 6.1 设备列表 `GET /api/admin/devices?key=`

响应 JSON：

```json
{"ok": true, "devices": [
  {"device_id": "d", "account": "u", "name": "n",
   "ip": "10.0.2.15", "last_seen": 1720000000, "created": 1720000000}
]}
```

### 6.2 下发命令 `POST /api/admin/command`

请求体 JSON：`{"key": "...", "device_id": "...", "type": "find|lock|wipe", "payload": "可选"}`

响应 JSON：`{"ok": true}`（下发失败返回 `{"ok": false, "error": "..."}`）。

### 6.3 查看备份 `GET /api/admin/backup?key=&device_id=`

响应 JSON：`{"ok": true, "files": [{"path": "...", "data": "<原始字节>"}]}`

### 6.4 管理后台页面 `GET /admin`

返回 HTML 单页（设备列表 + 下发 find/lock/wipe + 查看备份），
请求管理 API 时附加 `key`。

## 7. 设备端行为约定（KunxOS 客户机）

- **find**：执行「查找设备」= 立即 `ack`，结果 `r` 携带当前 IP
  （如 `ip=10.0.2.15`），服务端以此刷新设备 `ip/last_seen`。
- **lock**：写 `/data/oobe/locked=1`，客户机显示锁定界面；解锁写回 `0`。
  `ack` 结果 `r=done`（解锁时 `r=unlocked`）。
- **wipe**：删除 `/data/oobe` 全部内容（含 `.done`），设备回到首次启动
  状态；`ack` 结果 `r=wiped`。
- 客户机启动时自动 `poll` 一次；后续由用户点击「检查命令」触发。
