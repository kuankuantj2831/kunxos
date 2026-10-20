# 优化轮 041：类型线程 v2 复活——深路径 80→22µs 落袋，三连 sfsrace + 全门验证

- 日期：2026-10-20
- 轮次性质：自动化第 40 轮（r40 洗清嫌疑后的安全重启）
- 类型：syscall/存储性能（r37 prize 兑现）

## 实现（r37 补丁原样重放于 r38+r40 加固后的地基）

`kunxcore/src/vfs.rs`：DCacheEntry 增存子类型；`walk_from` cur_type 线程
传递（symlink 跳转后 T_UNKNOWN 哨兵，下轮顶实测一次）——每组件免两次
`fs_type` inode 读，miss 时一次 fs_type 供回填（原代码本就要读）。

## 量化（bench n=100k，getpid 对照 2.39µs 稳定）

| 操作 | r36（dcache） | r41（v2） | 深路径全程 |
| --- | --- | --- | --- |
| open+close 深路径 | 79,907 | **22,250 ns** | **126,168 → 22,250（-82%）** |
| open+close "/" | 19,066 | 18,101 | — |

## 验证（对照 r37 当年 2/3 失败的阴影，本轮全绿）

| 门 | 结果 |
| --- | --- |
| test-sfsrace ×3（含 r40 新协议） | **3/3 PASS** |
| lifecycle-core | 38/38 |
| KOS 三向拷贝 | PASS |

r37 的失败确系 r40 定证的 mailbox 认领窗（缓存提速→撞窗放大）——本案
闭环：修复协议后同款代码三连全绿。

## 深路径 open+close 三轮演进

126.2µs（基线）→ 79.9µs（r36 dcache）→ 22.3µs（r41 类型线程）= **-82%**。

## 遗留问题 / 下一轮建议（按优先级）

1. 余下 ~22µs 拆解：末分量 open 本体（inode 读+fd 表）为主，find_mount
   字符串前缀比对次之——下一档收益中位。
2. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
