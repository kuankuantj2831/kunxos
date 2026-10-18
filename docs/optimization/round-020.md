# 优化轮 020：virtio-sound 协议码根治（告警归零）+ 挖出「RISC-V 音频从未工作」被掩盖事实

- 日期：2026-10-18
- 轮次性质：自动化第 19 轮
- 类型：稳定性/协议正确性（virtio-snd 告警排查项，越挖越大）

## 本轮目标

排查每次启动都刷 QEMU stderr 的 `virtio snd header not recognized: 3`。

## 发现链（逐层取证）

1. **码值全错**：驱动 PCM 控制码误用顺序 3..9，规范是 0x0100..0x0105/
   0x0200 段（kunxcore/src/virtio_snd.rs 常量表）。QEMU 拒收 PCM_INFO(3)
   即告警本体。
2. **音频自 RISC-V 移植起从未工作**：verbose 启动实锤探测链——QEMU 对
   无法识别的码以**全零负载**应答 → `ch_max=0 < 2` → 「stream0 不可用于
   立体声输出」→ 探测返回 false → virtio-sound 从未初始化成功。
   test-audio 门自架构切换后未跑过，该事实被掩盖至今（启动提示音缺失
   无人上报；audiod 空转）。
3. 修码后：**告警 1→0**（实测），QEMU 识别并消费请求——但响应缓冲
   **40 字节全零**（新增 kinfo 级 dump 实证）：请求完成（used ring 更新）
   而 QEMU 的响应 DMA 从未写入驱动缓冲。

## 改动点

| 文件 | 改动 |
| --- | --- |
| `kunxcore/src/virtio_snd.rs` | PCM 控制码 3..9 → 规范 0x0100..0x0105 / 0x0200（JACK 1/2 本就正确） |
| 同 | PCM_INFO 后新增响应前 40B hex dump（kinfo 门控，每启动一行，诊断保留） |

## 验证

- 构建 0 错误；verbose 无头启动（带 virtio-sound-device）：告警数
  **1 → 0**；0 PANIC；串口 [vsnd] 探测日志完整（含响应 dump）。
- 诚实声明：**音频功能仍未恢复**——探测仍因全零响应失败（见遗留 1）；
  本轮是必要前置（协议层修正），`make test-audio` 门未跑（子系统已知
  down，跑了也是已知红，避免浪费一轮构建；修复后必须跑）。

## 遗留问题 / 下一轮建议（按优先级）

1. **PCM_INFO 响应 DMA 全零根因**（音频复活最后一关）：候选——legacy
   模式下响应描述符链不被 QEMU virtio-sound 写回（该设备通常 modern-only，
   驱动走 legacy 协商）；排查手段：`-trace enable=virtio_snd*` 对照、
   dump 控制队列描述符链、对照 net_virtio legacy 路径差异。修复后跑
   `make test-audio` + GUI 带音频冒烟。
2. 转场混合 ~10ms/帧（round-019 遗留，SWAR/降采样）。
3. EV 污染源监控（round-017 标记零触发中）。
4. syscall 微基准 / round-009 增益复核 / A1 fb EINVAL（长期）。
