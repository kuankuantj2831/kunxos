# 优化轮 042：open 路径冗余 inode 读去重——深路径 22→12.5µs、根路径 18→8.7µs

- 日期：2026-10-20
- 轮次性质：自动化第 41 轮
- 类型：syscall/存储性能（r41 余量拆解第一刀）

## 两处冗余（读 open_impl 定证）

1. `open_impl` 在 `resolve()` 后**再读一次 `fs_type(dev, ino)`**——walk
   刚算出的最终分量类型被 resolve() 丢弃（r41 类型线程只惠及 walk 内部）；
2. 对普通文件也无条件读 `fs_rdev(dev, ino)`——rdev 仅字符/块设备有意义。

## 修复（kunxcore/src/vfs.rs）

- `walk_from` 出口**带出最终类型**（返回三元组；终分量经 symlink 跳转的
  UNKNOWN 哨兵在出口实测一次），`resolve_path` 透传，`resolve()` 包装
  保持老签名（其余调用方零改动）；
- `open_impl` 改用 `resolve_path` 直取类型；`fs_rdev` 仅 `T_CHR/T_BLK`
  时读取。

## 量化（bench n=100k，getpid 对照 2.39µs 稳定）

| 操作 | r41 | r42 | Δ |
| --- | --- | --- | --- |
| open+close "/" | 18,101 | **8,731 ns** | **-52%** |
| open+close 深路径 | 22,250 | **12,468 ns** | **-44%** |

深路径全程演进：126.2 → 79.9（r36 dcache）→ 22.3（r41 类型线程）→
**12.5µs（r42 出口类型+rdev 门控）= -90%**。根路径 31µs → 8.7µs = -72%。
剩余 ≈ 双系统调用陷阱地板（getpid×2 ≈ 4.8µs）+ 必要 walk/表项工作，
下一档收益已薄。

## 回归

sfsrace PASS、lifecycle-core 38/38、KOS 三向 PASS（每跑含标记清理重建）。

## 遗留问题 / 下一轮建议（按优先级）

1. GUI 带音频冒烟（r41/r42 连续两轮内核改动后补一次画面级验证）+
   全门槛整合扫描（十门）。
2. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
