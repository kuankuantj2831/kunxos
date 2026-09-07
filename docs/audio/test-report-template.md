# KunxOS 音频测试报告模板

> 本模板与 `apps/audiolat/src/main.rs` 的 `=== audiolat report ===` 输出、
> `apps/audiomix` 的场景输出及内核 `audio::stats_dump` 的统计字段一一对应。
> 填写时直接粘贴工具输出，再补充「通过标准」判定。

## 0. 环境

| 项 | 值 |
|---|---|
| 目标平台 | QEMU virt（Cortex-A72） |
| QEMU 版本 | （如 11.1.0） |
| 音频后端 | `-audiodev wav,path=...`（或 none / pa） |
| 内核提交 | （git hash） |
| /dev/audio 后端 | virtio-sound / AC97 回退 |
| 采样格式 | 48kHz / 16bit / 立体声，period=10ms |
| 低延迟配置 | RING_CHUNKS=2，BUFFER_PERIODS=1，poll=2ms |

## 1. 场景矩阵

| # | 场景 | 说明 | 预期 | 实测 |
|---|---|---|---|---|
| 1 | 单应用播放 | `audiotest 2 440 5 1`（音乐持焦） | 正常发声，无 underrun | |
| 2 | 单应用直连 | `audiolat 5 --direct` | 仅内核路径延迟 | |
| 3 | 多应用混音 | `audiomix`（音乐+游戏同时播） | 两路叠加，可辨 523Hz + 659Hz | |
| 4 | 焦点抢占 | audiomix 中电话申请焦点 | 音乐/游戏被压音或静音 | |
| 5 | 延迟测量 | `audiolat 5`（完整路径） | 直方图集中 ≤30ms | |

## 2. 延迟直方图（audiolat 输出）

```
=== audiolat report ===
path: app->audiod->kernel           （或 kernel direct）
buffered latency histogram (write->DAC, samples=N):
  0-5 ms:    _ (__%)
  5-10 ms:   _ (__%)
  10-15 ms:  _ (__%)
  15-20 ms:  _ (__%)
  20-30 ms:  _ (__%)
  30-50 ms:  _ (__%)
  50-100 ms: _ (__%)
  >=100 ms:  _ (__%)
buffered min/avg/max: _/_/_ ms
```

### 内核 IRQ 统计

```
kernel IRQ stats (irq_count=_):
  IRQ period  min/avg/max: _ / _ / _ ms
  IRQ refill  min/avg/max: _ / _ / _ ms
  submit->consume (IRQ->DAC) min/avg/max: _ / _ / _ ms
  IRQ->DAC end-to-end (refill_avg+dma_avg): _ ms
underruns=_ late_writes=_
sw_frames=_ hw_frames=_
```

## 3. underrun / late_writes

| 指标 | 值 | 目标 |
|---|---|---|
| underruns | _ | 单应用连续播放 5s：≤ 3 |
| late_writes | _ | ≤ 5 |
| IRQ 周期 max/min | _/_ ms | max ≤ 15ms（RT 保障下） |

## 4. 延迟预算核算（回填审计）

| 环节 | 预算 | 实测 avg | 实测 max |
|---|---|---|---|
| audiod 拾取轮询 | 2ms | | |
| 混音相位 | 5ms | | |
| 内核环形缓冲 | 10ms | | |
| virtio-sound 在途 | 10ms | | |
| **合计** | **≈20ms（≤30ms 通过）** | | |

**判定**：<20ms 目标 □ 达标 / □ 未达标（未达标原因：……）

## 5. 结论与备注

（多应用混音是否可辨、焦点抢占是否生效、AC97 回退是否触发、A2DP 预留状态等）