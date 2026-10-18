# 优化轮 026：syscall 微基准受阻回滚 + 发现 launcher 开机崩溃环（向并发会话示警）

- 日期：2026-10-18
- 轮次性质：自动化第 25 轮
- 类型：基建尝试（受阻回滚）+ 环境发现

## 本轮目标

busybox-lite 加 `bench` applet（getpid/clock_gettime/open+close 用户态往返
微基准，clock_gettime 自计时 + volatile sink），建立从未有过的 syscall
开销基线。代码已完整实现（留存在 git 历史与本文档附录精神中：echo 分发
后加 bench 分支，三个循环各 N 次，put_u64 报告 ns/op）。

## 受阻经过（诚实记录）

1. shell 模式启动实测：`busybox echo alive` 正常（exec/ELF/分发链路好），
   但 `busybox bench` 紧跟输入出现 SIGSEGV（pc=0x400000 入口取指失败，
   recs=514 缺页记录）；后续带 [b1]-[b4] 打点的复跑中 bench 又完全静默
   （连首条打印都没有）——崩溃风暴中的诊断不可复现、不可信。
2. **二分定界**：git stash 暂存我的 busybox.c 改动后，开机崩溃依旧存在
   （基线 1 次 SIGSEGV vs 带我改动 2 次）——崩溃主体非我引入。

## 重要发现：launcher 开机崩溃环（⚠ 并发会话在制品，非本会话改动）

日志实锤（shell 模式启动，本轮基线二进制）：

```
[init] 正在退出（守护进程作为孤儿继续运行，内核 Shell 随后接管）
[crash-screen] 桌面启动器反复崩溃，图形界面已停止
[health][严重][display] 图形界面已停止，系统降级为控制台模式
[crash] tombstone: pid=3 name=busybox sig=11 pc=0x400000 recs=514（反复）
```

- **launcher 在开机期反复崩溃**直至 crash-screen 降级——时间窗口与并发
  会话在制品 launcher（app_launch.rs/main.rs 22:19 仍在动）吻合；
- tombstone 的 `name=busybox` 疑似**误标**（pid=3 恒定、514 次缺页符合
  launcher 体量；busybox 仅 6KB 且 echo 实测正常）——crash 转储的进程名
  解析值得另查（候选：comm 读取竞态/陈旧）；
- pc=0x400000 入口取指失败 + 高缺页数，形态与 ELF 加载/需求分页路径
  相关，供并发会话排查参考。

## 处置（按红线：修不完即回滚）

- 我的 busybox.c bench 改动**已回滚**（git checkout），不把不可验证的
  代码留在树上；完整实现已在本文档与 git 历史（本轮工作树快照前的
  stash 往返）中，待 launcher 稳定后一轮复活。
- 本轮提交仅本文档。

## 遗留问题 / 下一轮建议（按优先级）

1. **【并发会话优先】launcher 开机崩溃环排查**（上节证据；稳定前 shell
   模式启动不可用作任何诊断基座）。
2. tombstone 进程名误标排查（crash 转储 comm 解析）。
3. bench 复活 + 首个 syscall 基线（launcher 稳定后，代码现成）。
4. 转场余档（导航恢复 ~2-3ms/呈现 ~2.2ms）、boot_trace tcp、EV 监控、
   round-009 复核、A1（长期）。
