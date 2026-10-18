# 优化轮 032：大进程时钟成本实测（53µs/次）——radix 索引立项判决成立

- 日期：2026-10-19
- 轮次性质：自动化第 31 轮
- 类型：剖析/决策轮（round-031 结构性清单的先测后建）

## 本轮目标

r30/31 砍扫描轮数对小进程已兑现（busybox clock_gettime 31.6→14.4µs），
但大进程单次扫描仍 O(count)。在投入高风险索引结构前，先实测大进程
（OOBE，页记录 ~500+，历史 mono_us 41-50µs）在新内核下的真实成本。

## 新增观测点

`apps/oobe/src/main.rs` `load_fonts_sync` 尾部：一次性 mono_us×1000
自测（volatile sink 防优化），打印 µs/次。启动开销 +53ms 一次性，可忽略。

## 实测判决

```
[oobe] mono_us 自测: 53us/次
```

**53µs/次，对比历史基线 41-50µs 几乎未降**——数学吻合：syscall 底
~14µs + 单次 500 条线性扫描 ~35µs。结论：**radix/哈希辅助索引立项成立**
（OOBE 每动画帧调 mono_us 2-6 次，纯时钟开销最高 ~0.3ms/帧）。

## 索引立项规格（下轮实施清单）

- 结构：per-process vaddr→记录下标的 radix（用户 VA<1GB→两级即够）或
  开放寻址哈希，与 user_pages 数组并存（数组仍为权威，索引只加速查）；
- **失效边审计清单**（每处必须挂索引维护，漏一处=脏索引=错误校验）：
  ① add_page/add_page_with_pte/reserve_page（插入）
  ② process.rs 换位删除路径（记录删除）
  ③ copy_address_space 父进程 COW 降级（flags 变更）
  ④ mprotect_page（flags 变更）
  ⑤ execve 换地址空间 / 进程退出 / pid 槽复用（整表换血）
  ⑥ elf.rs 共享页并入 set_page_flags（r29 新增路径）
- 验收：OOBE mono_us 自测 53→<20µs；busybox bench 不回退；lifecycle×3
  + sfsrace + kos-copy 全绿。

## 遗留问题 / 下一轮建议（按优先级）

1. **radix/哈希索引实施**（上节规格；本轮判决依据 53µs 实测）。
2. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
