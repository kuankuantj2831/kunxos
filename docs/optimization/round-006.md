# 优化轮 006：lifd 命令信箱审计收口——生产早已迁移 seq 协议；回归门迁到真实 IPC 路径 + 新增背靠背连发回归

- 日期：2026-10-18
- 轮次性质：自动化第 5 轮
- 类型：回归基建 / IPC 正确性（round-005 头号遗留）

## 本轮目标

处理 round-004 发现、round-005 列为首位的 lifd cmd 信箱覆盖语义隐患：
「单文件 `cmd_<pid>` 覆盖式信箱，多条快速命令静默丢前一条（launcher 真实
场景可能同样踩）」。

## 审计结论（推翻了遗留项的前提）

**生产流量早已不存在该竞态。**全部三方生产写方均已迁移到
`cmd_<pid>_<seq>` 每消息一文件 + 写 `.tmp` 后 rename 原子发布（spool 语义，
目标名永不存在、rename 恒成功）：

| 写方 | 位置 | 证据 |
| --- | --- | --- |
| 应用侧 | `shared/lifecycle.rs` `lc_write_cmd` | 注释明确「消除 O_TRUNC 覆盖 × 读后 unlink 丢命令竞态」 |
| launcher | `apps/launcher/src/app_life.rs`（发布 FG/EXITED） | 「每消息一文件消除丢命令竞态」 |
| pushd | `kunxserv/pushd/src/main.rs`（转发 DELIVER） | `cmd_<pushd_pid>_<seq>` + tmp/rename |

lifd 读侧（`poll_mailbox` 扫描 `cmd_*` + `parse_dec` 到非数字停止）对
`<seq>` 后缀零改动兼容。**真正还踩旧协议的只有两个测试脚本**——即
round-004 的假失败其实发生在生产已废弃的路径上，而回归门从未覆盖生产
真实 IPC。

## 改动点

| 文件 | 改动 |
| --- | --- |
| `tests/scripts/lifecycle_core_test.py` | 全部 11 处命令写改为 `run_cmd(pid, text, marker)`：`echo <text> > cmd_<pid>_<seq>.tmp` + `mv` 原子发布（与 `lc_write_cmd` 同款）；回执等待保留 |
| `tests/scripts/lifecycle_test.py` | 同款迁移：新增 `cmd_pub`/`cmd_wait`，`cmd_to` 与 BTASK/SET_TIMEOUT 全部站点改走 seq 协议 |
| `tests/scripts/lifecycle_cmd_burst.py` | 新增微型回归：**背靠背三连发**（不等任何回执，round-004 病灶同款节奏）3 条 BTASK，断言 lifd 全部消费（2 授予 + 1 每应用上限拒绝）。旧协议在该节奏下实测丢命令（round-004 日志：A2 被 A3 覆盖消失） |

## 验证

```
lifecycle_core_test.py  → 38/38 passed
lifecycle_test.py       → 52/52 passed
lifecycle_cmd_burst.py  → grants=2 per-app-denies=1 → PASS
```

三个用例共 4 次完整 QEMU 启动（shell 模式，服务全部拉起，无 PANIC）。
本轮未改任何产品代码，无需重建内核；脚本走生产同款 `echo > .tmp` + 内置
shell `mv`（rename）路径，回归门从此覆盖真实 IPC 协议。

## 遗留问题 / 下一轮建议（按优先级）

1. memmove 重叠路径反向逐字节循环（round-001 遗留，已顺延多轮）。
2. memcpy 混合对齐形态 (0,4)/(4,0)（round-001 遗留）。
3. OOBE 步骤转场（200ms 淡入淡出）帧率复核：是否也有 round-005 同款
   过时节流（空闲动画已解锁 100fps，转场路径未查）。
4. kos/src/error.rs:143 unused variable 警告清零（round-001 遗留）。
5. `make lifecycle-test` 门槛可纳入 burst 脚本（Makefile target 追加一行）。
6. A1 fb EINVAL 待自然复现（BUG_REPORT 档案）。

## 环境坑（新增）

- subprocess `cwd=临时目录` 时 KERNEL 必须先 `os.path.abspath`（本轮
  burst 脚本首版踩中：QEMU 秒退 + stderr 被吞 + python stdout 块缓冲
  → 外层 timeout 静默 124）。测试脚本 print 记得 flush。
