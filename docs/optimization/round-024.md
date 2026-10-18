# 优化轮 024：音频复活后全门槛八门扫描全绿 + 正常路径冒烟 + 测试标记泄漏坑

- 日期：2026-10-18
- 轮次性质：自动化第 23 轮
- 类型：回归整合轮（round-023 头号遗留：音频正常路径验证；内核音频路径改动后的全量回归）

## 本轮目标

1. virtio-sound 修复后的 GUI 正常路径（非 verbose）带音频冒烟。
2. 内核改动后的全门槛回归扫描（六个既有门 + perftest）。

## 正常路径冒烟（GUI 实例，quiet 模式，带 virtio-sound）

- **audiod 以真设备启动**：`/dev/audio 48000Hz 2ch 16bit`、焦点策略就绪
  （修复前该服务空转无设备）。
- 0 PANIC；OOBE 正常推进；[perf] 空闲间隔 avg 6.6ms（~150fps 呼吸，
  round-012 后水平维持）；QEMU stderr 告警 0；截图像素统计健康
  （770 色/Top1 28%/行多样性 2-65 结构化，/tmp/kunxos-vnc-r24.png）。

## 全门槛扫描（八门）

| 门槛 | 结果 |
| --- | --- |
| SFS RACE + MAILBOX（make test-sfsrace） | **PASS** |
| KOS 三向拷贝（make test-kos-copy） | **PASS** |
| lifecycle-core | **38/38** |
| lifecycle 完整 + burst（make lifecycle-test） | **52/52 + PASS** |
| perftest A/B/C/D/D2/E（带音频设备） | 全部 nominal（A 2409fps、D 100%、D2 183µs、E 65µs——与近轮一致） |
| test-audio（round-023） | **PASS**（架构切换后首次） |
| GUI 带音频冒烟 | 本文上节 |
| virtio-snd QEMU 告警 | **0**（修复前每次启动刷屏） |

## 新坑（本轮抓出并修复）：测试标记泄漏

**外层 `timeout` 杀掉 make 测试目标会跳过标记清理**：round-023 的
`timeout 420 make test-audio` 在 make 清理 `/etc/audio-test` 前的窗口内
被杀——标记虽被 make 自身清掉，但 **kernel.img 仍嵌着标记时代的
ramdisk**，本轮 GUI 冒烟因此串行跑了 audiomix（Makefile 注释警告过的
失败模式变体）。处置：touch ramdisk 重建内核，验证镜像中标记已不在
（无头启动 grep 0 确认）。规矩固化：**凡 timeout 跑过的测试目标，之后
必须 touch ramdisk 重建内核并验证标记不在**。

## 改动点

无产品代码（纯验证轮）；kunxcore/ramdisk.img + kernel.img 重建同步
（标记清除后的干净镜像）。

## 遗留问题 / 下一轮建议（按优先级）

1. 转场混合 ~10ms/帧（round-019 遗留，SWAR/水平降采样，需像素自检；
   注意并发会话在动画领域活跃，动手前查在制品）。
2. boot_trace pipe 串口偶发早夭（改 -serial tcp）。
3. EV 污染源监控 / syscall 微基准 / round-009 增益复核 / A1 fb EINVAL
   （长期遗留）。
