# 优化轮 033：自证式单槽位置缓存——大进程时钟 53→5µs（10.6x），clock_gettime 三轮累计 -77%

- 日期：2026-10-19
- 轮次性质：自动化第 32 轮
- 类型：syscall 性能（round-032 立项施工，设计升级为零失效边）

## 设计（比立项规格更安全）

原计划 radix/哈希 + 六条失效边手工审计。施工前直写点清点带来设计升级：
**自证式单槽位置缓存**（cache 只存「最近命中的记录下标」）：
- 命中条件 = `idx < count && user_pages[idx].vaddr == 目标页`——**现读记录
  复核**，flags/paddr 永不陈旧（每次从记录现读）；
- 追加（add/reserve）不迁移既有记录 → 缓存天然保持有效；
- 换位删除/截断/清表都会破坏复核条件 → **自然失效**退回全表扫描重填；
- **零代际计数、零手工失效钩子**——漏维护的故障模式不存在，最坏情况
  是一次缓存 miss。热模式（同页反复校验，如 clock_gettime 恒写同一下
  栈页）O(1) 命中。

## 改动点

| 文件 | 改动 |
| --- | --- |
| `kunxcore/src/vm.rs` | `ProcPageTable` 新增 `cache_idx`（usize::MAX=冷），构造点初始化 |
| `kunxcore/src/process.rs` | `lookup_page_idx` 快路径（复核命中 O(1)，miss 全表扫描回填） |

## 量化

| 场景 | r29 基线 | r31 | **r33** | 三轮累计 |
| --- | --- | --- | --- | --- |
| OOBE mono_us 自测（大进程 500+ 记录） | 41-53 | 53 | **5 µs/次** | **10.6x** |
| busybox clock_gettime | 30,640-31,677 | 14,387 | **7,281 ns** | **-77%** |
| getpid（对照） | 3,543 | 3,497 | 3,479 | 稳定 ✓ |

5µs 与陷阱地板（3.5µs）+ 缓存命中的数学完全吻合。

## 回归

lifecycle-core **38/38**（fork/exec/mprotect/COW 大量记录变异 = 缓存
失效路径压力测试）；GUI 带音频冒烟 0 PANIC、OOBE 正常启动。

## 遗留问题 / 下一轮建议（按优先级）

1. 其余 O(n) 扫描点同款缓存化候选：`page_fault_resolve`/`cow_break` 的
   LAZY/COW 查找（每缺页一次全表扫）、`root_page_recorded`（剩余调用方）。
2. open+close 31.7µs 仍偏重（VFS 双锁+路径解析），下一个 syscall 目标。
3. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
