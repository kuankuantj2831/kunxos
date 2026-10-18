# 优化轮 028：bench 复活 + 抓到 SIGSEGV 金矿证据——内核装载路径「同 VA 重复页登记」（dbl-track 实锤）

- 日期：2026-10-18
- 轮次性质：自动化第 27 轮
- 类型：基建（bench 落地）+ 稳定性取证（round-026/027 悬案的突破）

## bench 复活

`busybox bench [n]` 重放落地（round-026 留存代码）：getpid /
clock_gettime / open+close 三条用户↔内核往返微基准，MONOTONIC 自计时。
**首跑仍未成功**——但失败本身炸出了下述金矿（bench 代码无罪：崩在
exec 装载路径，进入 main 之前）。

## 金矿证据（verbose shell 启动，`busybox bench` 触发，/tmp/bench-r28.log）

```
busybox bench
[diag] 加载 len=7424 magic=0x464c457f          ← ELF 本体正常
[diag] data ptr=0x83173020 v64-entry=0x1        ← 入口读出 1？！（应为 0x400000）
[perf] ELF 加载 pages=2 / 启动进程 /bin/busybox
[pf-diag] va=0x400000 tid=4 pid=3 recs=514      ← 入口取指失败
[fault] pid=3 elr=0x400000 mapped=1 rec=0x83176000  ← mapped=1 仍取指失败！
[dbl-track] proc 3 存在 1 条同 VA 重复页记录（退出时会双重释放，需排查登记来源）
```

三条关键线索全部指向**内核 ELF 装载/页登记路径**：
1. `v64-entry=1`：装载器某处读入口字段得到 1（读错偏移/未初始化/竞态）；
2. `mapped=1` 却取指失败：页表登记了但映射不可执行/被二次操作破坏；
3. **`[dbl-track] 同 VA 重复页记录`**：同一虚拟页被登记两次（双重释放
   风险 + 第二次登记大概率覆盖/破坏第一次的映射）。

### 连环案并档（round-026/027 悬案就此串起来）

- pid=3 name=busybox **不是误标**（tid=4 pid=3 st=2 busybox 实证）——
  是 pid 槽位复用后 shell fork 的 bench 进程；
- round-027「verbose 零 SIGSEGV」只是运气（本轮 verbose 也崩）；
- **同启动 `busybox echo` 存活、`busybox bench` 必崩**——同二进制同
  装载路径不同命运，差异在 exec 时的内核侧状态（pid 槽位史/页登记
  残留），与 round-017 OOBE EV 离奇越界**同族嫌疑升级为强嫌疑**：
  「exec/页登记路径的状态残留污染」。

## 处置

- bench 代码**保留在树**（观测催化剂 + 未来基线工具；其 C 实现无罪）。
- 内核装载路径修复 = 下轮头号（[diag]/[dbl-track] 探针已在 HEAD 里，
  应为并发会话或夜间工单留下的排查装具——顺着它挖）。

## 遗留问题 / 下一轮建议（按优先级）

1. **exec 页登记重复根因修复**（elf.rs/loader + 页跟踪：查 v64-entry=1
   的读取路径、双登记来源、pid 复用时 slot 清理；修好后 bench 首跑 +
   round-017 EV 悬案大概率连带告破）。
2. bench 首个 syscall 基线（修复后立即）。
3. 转场余档 / boot_trace tcp / A1（长期）。
