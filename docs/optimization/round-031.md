# 优化轮 031：页校验单次带下标查找 + LAZY 原位落地——clock_gettime 两轮累计 -54%（31.6→14.4µs）

- 日期：2026-10-19
- 轮次性质：自动化第 30 轮
- 类型：syscall 性能（round-030 结构性清单的第一步）

## 改动

`ensure_user_range_writable` 每页原本跑 **2-3 轮全表线性扫描**：
`user_range_mapped`（记录扫描+PTE 漫游）→ LAZY 再 `page_fault_resolve`
（第三轮重扫）→ `current_page_flags`（又一轮）。改为：

- `lookup_page_idx(page) -> Option<(下标, 记录)>` **单次扫描**；
- LAZY 记录 **按下标原位落地**（`page_fault_resolve_at`，分配+清零+映射+
  记录更新语义与原版逐行一致；下标失效防御性退回全表路径）；
- 无记录页走 `user_page_pte_u` PTE 漫游放行（SHM/framebuffer，与
  user_range_mapped 共享映射路径同语义；该函数改 pub）。

LAZY 语义保持 root_page_recorded 同款（LAZY=未映射）；落地保留原 flags，
rec.flags 判断 COW/只读继续有效。

## 量化（bench n=100k，getpid 对照跨启动稳定性验证）

| 操作 | r29 基线 | r30 | r31 | 归一化（÷getpid） |
| --- | --- | --- | --- | --- |
| getpid（对照） | 3,543-3,618 | 4,063 | **3,497**（宿主回落可比） | 1x |
| clock_gettime | 30,640-31,677 | 26,452 | **14,387** | 8.8x → **4.1x（累计 -54%）** |
| open+close | 31,440-35,883 | 39,119(漂移) | 32,849 | 同步受益 |

## 回归

lifecycle-core **38/38**（LAZY 落地/COW/mprotect 路径全覆盖）；GUI 带音频
冒烟 0 PANIC、OOBE 正常、转场 13 帧/相位正常（混合 6.6ms 维持）。

## 遗留问题 / 下一轮建议（按优先级）

1. **记录表 O(count) 仍在大进程付全额**（busybox 表小故本轮收益已近饱和；
   OOBE/launcher 500+ 页进程的单次扫描仍 ~O(500)）——radix/哈希辅助索引
   为结构性下一档（本轮已把扫描轮数砍到 1，索引落地后即 O(1)）。
2. mono_us 用户态复核（大进程场景，索引落地时一并测）。
3. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
