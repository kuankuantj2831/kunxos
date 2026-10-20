# 优化轮 039：mailbox 冻结定证——dones=2 从 polls=6000 冻到 30000，认领协议「迟到哨兵窗口」定位

- 日期：2026-10-20
- 轮次性质：自动化第 38 轮（r38 头号遗留的定向跟进）
- 类型：稳定性定证（无代码改动轮）

## 推翻 r38 的「余量」假设（心跳数据免费定证）

r37 嵌入式失败日志的心跳线（/tmp/kx-r37.log，仍在）：

```
[mbx-dbg] polls=6000  dones=2
[mbx-dbg] polls=12000 dones=2
[mbx-dbg] polls=18000 dones=2
[mbx-dbg] polls=24000 dones=2
[mbx-dbg] polls=30000 dones=2   ← 超限
```

**dones 冻结在 2**：第三写者的 DONE 哨兵永远没到——不是截止期余量
（慢写者会推进），是哨兵**丢失**。+20% 余量的二分方案作废。

## 机制定位（读 mbx_reader/mbx_writer 协议定证）

认领协议（apps/sfsrace/src/main.rs:656-692）：

```
rename(pipe→taken)              // 读者原子认领
open(taken) → read_all → close  // 读走当前内容
unlink(taken)                   // ★ 窗口
```

写者持**长命 fd** 追加（rename 后写入仍落在 taken 的 inode 上）。若写者
的最后一条 DONE 追加恰好落在 `read_all` 与 `unlink` 之间——哨兵在 taken
内但已被读过，unlink 将其删除→**DONE 永久丢失→dones 冻结** ✓ 与心跳
形态完全吻合。

协议的防御依赖一条内核语义：**「写者持 fd 时 unlink(taken) 失败」→
TAKEN_LEFT=true → 下轮先排干再删**。dones 冻结 = 该防御在特定时序下
被绕过（候选：写者恰好在本条 open→open 间隙无 fd；或 unlink 语义在
某时序下对持 fd 文件成功返回）。

## 处置与 v2 计划（本轮不抢修——按红线）

无代码改动（分析轮）。v2 定证三件套：
1. 写者 fd 生命周期打印（长命 or 每条一开一关——决定窗口宽度）；
2. TAKEN_LEFT 每次翻转打印（失败轮回放其转换序列）；
3. 失败复现保持 5 连法（r38 已知 ~1/5 复现率）。
修复方向预案：unlink 前二次排干（read_all 循环到 0），或 taken 改用
「rename 到唯一名 + 延迟一拍删除」。

## 遗留问题 / 下一轮建议（按优先级）

1. **mailbox v2 三件套定证 + 修复**（本档方案）。
2. 类型线程 v2（r37 档案；与本案独立）。
3. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
