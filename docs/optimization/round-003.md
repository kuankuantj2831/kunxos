# 优化轮 003：lifecycle 回归落地 RISC-V——揪出 lifd 墙钟漏改（挂起永久失效）

- 日期：2026-10-18
- 轮次性质：自动化第 2 轮
- 类型：稳定性（真实缺陷修复）+ 回归基建修复

## 本轮目标

执行 round-002 头号遗留：跑 GUI lifecycle-test 回归。落地时发现更深一层的问题：
两个 lifecycle 脚本自 2026-09-18 RISC-V 架构切换起**从未被更新**（硬编码
`qemu-system-aarch64 -cpu cortex-a72 -device AC97`），端到端回归门名存实亡
一个月。本轮先让回归真正可跑，跑出来的失败里再分真伪。

## 改动点

| 文件 | 改动 |
| --- | --- |
| `tests/scripts/lifecycle_test.py`、`tests/scripts/lifecycle_core_test.py` | QEMU 调用 RISC-V 化：riscv64 + OpenSBI `-bios` + `-cpu rv64`，AC97（aarch64 遗留）换 virtio-sound；QEMU/BIOS 路径与顶层 Makefile 同源，支持环境变量覆盖 |
| `kunxserv/lifd/src/main.rs` `now_us()` | **SYS_GETTIMEOFDAY（墙钟）→ SYS_CLOCK_GETTIME(CLOCK_MONOTONIC)**（app_kit 同款写法，纳秒→微秒换算）。夜间工单「黑屏事故同族防御」清剿时 lifd 漏网 |
| `tests/scripts/lifecycle_core_test.py` | 时序健壮化：BTASK 时长 500000→10000000（慢宿主下命令往返 >0.5s，短任务到期太快凑不满每应用上限=假失败）；挂起等待窗 4s→15s |

## 真实缺陷与机理

lifd `now_us()` 读墙钟。RTC 就绪/校时导致墙钟回拨时，
`now.saturating_sub(e.last_used_us) >= BG_TIMEOUT_US` 恒为 0——**「后台→
挂起」超时永久失效**（后台应用永不休眠，白耗内存/CFS 份额）。夜间工单已把
各应用 now_us 本体改 CLOCK_MONOTONIC（墙钟跳变黑屏事故根治），lifd 是漏网
之鱼；因回归脚本架构漂移无法运行，一直没被抓住。

## 验证

### 归因对照实验（诚实性要求）

| 构建 | 测试参数 | 结果 |
| --- | --- | --- |
| 旧 lifd（墙钟） | 旧参数 | 35/38（btask2 denied / appA suspended / recents S 失败） |
| **新 lifd（单调钟）** | 新参数 | **38/38 全绿** |
| 旧 lifd（墙钟，git stash 对照） | **新参数** | **35/38（挂起仍失败）** |

对照行证明：挂起修复归因于 CLOCK_MONOTONIC 而非放宽等待窗；btask 失败
归因于任务时长时序（机制显然：0.5s 任务在 RISC-V TCG 往返期即过期）。

### 构建与冒烟

`make kernel` 0 错误（lifd 经 ramdisk 依赖链重编打包，kernel.img 16:08:45）。
核心回归一次运行 = 完整内核启动 + init 拉起 lifd/pushd/audiod/sensord +
串口 shell 驱动 38 项状态机断言（R/B/S/T、SIGSTOP/CONT、后台任务白名单/
上限/到期、超时挂起、LMK、通知延迟补投），全部通过；日志无 PANIC。本轮
未触碰合成器/UI，无需截图。

### 完整版 lifecycle_test.py 诊断（34/56，非缺陷）

失败项全部为**期望文案漂移**：脚本等英文标记（`bg task denied`、
`pressure Moderate`、`[lifd] suspend`、`background appA`），lifd 现打中文
（`后台任务被拒`、`内存压力中等`、`已挂起`、`已转入后台`）。等价覆盖已由
核心测试承担；完整版需一次标记刷新（机械映射，见下轮建议①）。

## 遗留问题 / 下一轮建议（按优先级）

1. **lifecycle_test.py 文案刷新**：56 项检查的期望串按当前 lifd 中文文案
   映射更新（对照 lifecycle_core_test.py 已验证的正确标记），跑绿后作为
   `make lifecycle-test` 门。注意 Makefile 该 target 仍指向此脚本。
2. OOBE/应用帧节奏剖析（round-002 遗留：渲染 40µs 但帧间隔 19.7ms）。
3. memmove 重叠路径反向逐字节循环（round-001 遗留）。
4. memcpy 混合对齐形态 (0,4)/(4,0)（round-001 遗留）。
5. kos/src/error.rs:143 unused variable 警告清零（round-001 遗留）。
6. A1 fb EINVAL 待自然复现（BUG_REPORT 档案）。

## 环境坑（新增）

- 无（本轮沿用既有坑清单：rustup 代理走 make、pkill 影子化用 kill 直杀）。
