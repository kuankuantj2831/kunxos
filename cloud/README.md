# KunxOS 云服务

把 KunxOS 的占位云服务做成真实端到端云服务。架构三端：

```
┌─────────────┐  明文 HTTP          ┌───────────────────────────────┐
│ KunxOS 客户机 │ ────────────────▶ │ kunxoscloud.kuankuantj.com   │
│ (apps/cloud) │  http://域名/…     │   └─ Cloudflare Worker + D1   │
└─────────────┘                    └───────────────────────────────┘
                                        ▲ 管理后台 /admin（HTTPS）
```

- **生产（无 VPS）**：客户机经明文 HTTP 直连 Worker 的自定义域名
  `kunxoscloud.kuankuantj.com`（zone 关闭 Always Use HTTPS 后 80 端口不跳转）。
- **可选网关** `cloud/gateway/gateway.py`：仅当你在 VPS 上想要一个中转
  端点时使用（生产反代 Worker / `--local` 本地 JSON 存储联调）。
- **联调（QEMU）**：本地跑 `gateway.py --local`，客户机默认服务器
  `10.0.2.2:8080`。

## 组件

| 组件 | 路径 | 说明 |
|---|---|---|
| API 契约 | `cloud/PROTOCOL.md` | 客户端 ↔ 服务端协议（权威，三端严格遵循） |
| 服务端（生产） | `cloud/worker/` | Cloudflare Workers + D1：设备注册 / 备份 / 恢复 / find-lock-wipe 命令 / 管理 API + Web 后台 |
| 明文网关（可选） | `cloud/gateway/gateway.py` | OS 客户机无 TLS；VPS 中转或本地联调 |
| 客户机 | `apps/cloud/` | EL0 云服务应用：注册、备份上传、云端恢复、轮询并执行 find/lock/wipe、GUI |
| 端到端测试 | `tests/scripts/cloud_e2e.py` | QEMU 内驱动 OOBE→桌面→云服务应用→注册/备份→admin 下发 lock/wipe→OS 执行 |

## 生产部署（Cloudflare 侧，已完成）

1. `wrangler d1 create kunxos-cloud` → database_id 填入 `wrangler.toml`
   （当前已用 `fb2000e0-3ea1-489c-bfdc-d6725926eb49`）。
2. `wrangler d1 execute kunxos-cloud --remote --file=schema.sql`（幂等建表）。
3. `wrangler secret put ADMIN_KEY`（管理密钥；secret 与 var 不能同名，
   因此 wrangler.toml 不再声明 `[vars] ADMIN_KEY`）。
4. `wrangler deploy --domains kunxoscloud.kuankuantj.com` 绑定自定义域名。
5. zone（kuankuantj.com）关闭 **Always Use HTTPS** 与
   **Automatic HTTPS Rewrites**，使 80 端口明文请求直达 Worker。

## 客户机连接配置

客户机内核 TCP 栈无 TLS，只能明文 HTTP；`apps/cloud` 的服务器地址支持
**IP 或域名**（新增 syscall 403 DNS 解析，经 QEMU slirp → 宿主 DNS）。

- QEMU 联调默认：`/data/oobe/cloud_server` = `10.0.2.2:8080`（本地网关）。
- 生产：改成 `kunxoscloud.kuankuantj.com`（端口默认 80）。
- 修改方式：直接编辑 `/data/oobe/cloud_server`，或首次 OOBE 勾选云服务后
  手动改写该文件（OOBE 当前写默认值 `10.0.2.2:8080`）。

## 管理后台

- Worker 直连：`https://kunxoscloud.kuankuantj.com/admin`（密钥 ADMIN_KEY）。
- 或本地网关：`http://127.0.0.1:8080/admin`。

## 已知问题

- QEMU `-display none` 下 AC97 DMA 不完成，内核 `play_tone` 卡死启动
  （与云服务无关；E2E 脚本已去掉 `-device AC97`）。
- 若内核处于未完成的重构状态（ELF 加载器 Box 化），`loading /init` 可能
  PANIC，需先让内核可启动再跑 E2E。
