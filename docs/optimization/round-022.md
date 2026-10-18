# 优化轮 022：virtio-sound 响应管道打通——DESC_F_NEXT 缺失根治，RISC-V 首次收到设备响应

- 日期：2026-10-18
- 轮次性质：自动化第 21 轮（轮号 021 被并发会话用户直指令轮占用）
- 类型：稳定性/功能修复（round-020 头号遗留：响应 DMA 全零根因；提交 97a7bcd）

## 根因链（逐层实证）

1. **used ring 完成项 dump**（本轮新增诊断）揭示：`ctl 完成: id=0 len=4`
   ——设备只消化了 16B 请求段、响应写入量仅 4。
2. **真凶**：`ctl_send` 的请求描述符 flags=0，**从未设置 DESC_F_NEXT**
   （全文件连常量都没定义；控制链与 tx 数据链 h→d→s 三段全断）——设备
   把请求当独立单描述符缓冲，**链上的可写响应描述符对设备永不可见**，
   响应 DMA 自然永远不发生。这同时解释了 round-020 的全部谜团（修复前
   QEMU 的 BAD_MSG 响应也从未落袋、缓冲恒零、st 恒读 0 被当 OK）。
3. 修链后响应到达（len=36 实测），又暴露**第二层**：响应头解析错位——
   规范 `virtio_snd_hdr = {code(4), status(4)}`，驱动把 [0..4] 当状态
   （0x8000 段被当 st=32768）。改取 [4..8] + resp_len 4→8/36→40 后：
   **状态=OK，formats 位图 0x3fff 等真实能力数据首次现身**。

## 改动点

| 文件:位置 | 改动 |
| --- | --- |
| `kunxcore/src/virtio_snd.rs` | 新增 `DESC_F_NEXT`；控制链请求描述符、tx 链 xfer 头/数据段描述符补设 NEXT |
| 同 `ctl_send` | 状态码改取响应头 [4..8]（规范布局）；resp_len<8 拒绝 |
| 同 | `ctl_simple`/`set_params` resp_len 4→8；`pcm_info` 36→40 |
| 同 | used ring 完成项 dump（id/len/resp0，kinfo 门控）诊断保留 |

## 验证

- 构建 0 错误；verbose 无头启动：告警 0、PANIC 0。
- 控制消息往返实测：`ctl 完成: id=0 len=36`（修复前 len=4 零写入）；
  PCM_INFO 状态 OK、载荷出现真实位图（formats 0x3fff 类特征值）。
- **诚实声明：探测仍未通过**——pcm_info 载荷字段存在 +4 字节错位
  （direction/ch 解析位仍取到 0），音频未复活；见遗留 1。

## 遗留问题 / 下一轮建议（按优先级）

1. **pcm_info 载荷字段对齐定证**（音频复活最后一格）：响应 40B 原始
   dump 已在（formats 特征 0x3fff 落在 [20..28]、疑似 rates 在 [28..36]
   但位图不含 48000 位，存在两种对齐假说）。手段：对照 QEMU
   hw/audio/virtio-snd.c 的响应构造逐字段核对，或按特征值穷举两种偏移
   写死自检。修好后跑 `make test-audio` + GUI 带音频冒烟。
2. 转场混合 ~10ms/帧（round-019 遗留）。
3. EV 污染源监控 / syscall 微基准 / round-009 增益复核 / A1（长期）。
