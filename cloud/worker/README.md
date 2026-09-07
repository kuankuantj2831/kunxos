# KunxOS 云服务 · Cloudflare Workers + D1

KunxOS 云服务的生产端实现：Cloudflare Workers + D1 数据库。
响应格式与 `cloud/gateway/gateway.py`（本地模式）完全一致，权威契约见 `cloud/PROTOCOL.md`。

## 文件

- `wrangler.toml` — Worker 配置（D1 绑定 `DB`、`ADMIN_KEY` 变量）
- `schema.sql` — D1 建表 SQL（devices / backups / commands）
- `src/worker.js` — Worker 实现（设备 API + 管理 API + 管理后台页面）

## 部署步骤

前置：安装并登录 [Wrangler](https://developers.cloudflare.com/workers/wrangler/)：

```bash
npm install -g wrangler
wrangler login   # 或设置 CLOUDFLARE_API_TOKEN 环境变量
```

1. **创建 D1 数据库**

   ```bash
   wrangler d1 create kunxos-cloud
   ```

   把输出中的 `database_id` 填入 `wrangler.toml` 的 `[[d1_databases]]` 段
   （`database_id = "..."`）。

2. **初始化表结构**

   ```bash
   wrangler d1 execute kunxos-cloud --remote --file=schema.sql
   ```

3. **部署 Worker**

   ```bash
   wrangler deploy
   ```

   记下输出的 Worker URL，形如 `https://kunxos-cloud.<你的子域>.workers.dev`。

4. **设置管理密钥**（覆盖 `wrangler.toml` 里 `[vars]` 的 `change-me` 占位值）

   ```bash
   wrangler secret put ADMIN_KEY
   ```

   输入任意强随机字符串。同名 secret 会覆盖 toml 中的 var。

5. **指向明文网关（生产模式）**：KunxOS 客户机只有明文 HTTP TCP 栈，
   用 gateway.py 反代到 Worker（HTTPS）：

   ```bash
   python3 cloud/gateway/gateway.py \
       --backend https://kunxos-cloud.<你的子域>.workers.dev \
       --port 8080 --admin-key <与 ADMIN_KEY 相同的密钥>
   ```

   QEMU 客户机经 `10.0.2.2:8080` 访问网关（明文 HTTP），网关原样转发到 Worker。

6. **管理后台**：浏览器打开 `http://127.0.0.1:8080/admin`
   （或直连 Worker `https://kunxos-cloud.<你的子域>.workers.dev/admin`），
   密钥填第 4 步设置的 ADMIN_KEY。

## 本地开发（可选）

不部署 Cloudflare，用网关本地模式（JSON 文件存储，契约一致）：

```bash
python3 cloud/gateway/gateway.py --local --port 8080 --admin-key admin
```

## 备注

- 设备 API 为纯文本行协议；令牌不落库，仅存 FNV-1a 64 位哈希（`h()`）。
- 备份为整快照替换：`POST /api/backup` 先 DELETE 再 INSERT 该设备的全部文件。
- `GET /admin` 管理页面请求相对路径 `/api/admin/*`，Worker 与网关均天然兼容。
