# 优化轮 043：r41/r42 后整合扫描十门全绿 + lifecycle ps_pid 竞态修复 + 转场 21 帧复合红利

- 日期：2026-10-20
- 轮次性质：自动化第 42 轮
- 类型：回归整合 + 测试基建修复

## lifecycle-full 21 项连锁假失败定证与修复

首轮 lifecycle-test 31/52：`test pids` 起全崩。双模式快速复现（verbose/
quiet shell 各一次 `ps`）**均正常**——排除 r42 回归，定位为
`ps_pid()` 的**固定 sleep(0.4) 竞态**：宿主抖动下一次慢 ps 输出取空 →
三 pid 全 None → 21 项连锁超时（lifecycle-core 同款逻辑用 1.0s 从未
踩中）。修为**轮询解析 2s**（tests/scripts/lifecycle_test.py），复跑
**52/52 + burst PASS**。

## 十门整合扫描（r41 类型线程 + r42 open 去重后全量复核）

| 门 | 结果 |
| --- | --- |
| sfsrace / kos-copy / lifecycle-core | PASS（r42 轮已跑）|
| lifecycle 完整 + burst | **52/52 + PASS**（本轮）|
| perftest A/B/C/D/D2/E | 8 场景全 nominal |
| test-audio | **PASS** |
| GUI 带音频冒烟 + VNC 截图 | 0 PANIC、OOBE 正常、mono_us 3µs、**转场 21 帧**、snd 告警 0、像素统计健康（771 色/2-65）|

## 复合红利观测（无新改动，系统自身变快）

- 转场 **21 帧 / 175ms（≈120fps）**——r25 水平半幅落地时 14 帧，
  syscall 线（时钟 31.6→2.8µs、open 31→8.7µs）把转场循环的每帧开销
  再压一档，全栈复利显现；
- mono_us 自测 3µs（轻载宿主 + r33 缓存）。

## 遗留问题 / 下一轮建议（按优先级）

1. 候选池回补（近期聚焦 syscall/存储线）：launcher 交互 t2f 延迟观测
   （boot_trace 输入时间戳）、缺页/COW 查找缓存化（低优先）、EV 监控、
   转场余档（导航/呈现各 ~2-3ms）、boot_trace tcp、A1。
