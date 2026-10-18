# 优化轮 023：RISC-V 音频复活收官——请求体布局定证，五层 bug 全剥完，test-audio 首过

- 日期：2026-10-18
- 轮次性质：自动化第 22 轮
- 类型：功能修复（round-022 头号遗留：pcm_info 对齐定证 → 演变为全链贯通）

## 本轮战果

**RISC-V 架构切换以来音频子系统首次真正工作**：

```
[vsnd] stream0 dir=0 ch=1..2 fmt=0xe0078 rates=0x3fff   ← 真实能力
[vsnd] SET_PARAMS → 0x8000 OK / PREPARE → OK
[audio] /dev/audio 就绪，后端=virtio-sound 48kHz/16bit/立体声
[audio] 启动提示音 → [vsnd] 播放已启动
make test-audio → AUDIO TEST: PASS（audiomix 混音/焦点 + audiolat 延迟）
```

## 本轮剥掉的三层（连同前两轮共五层）

1. **响应头布局**（WebFetch QEMU 源码定证）：响应头是**单 le32 状态码**
   （无独立 code/status 对），载荷紧随 @resp[4..]。驱动载荷字段相对偏移
   本就正确（formats@+8/rates@+16/dir@+24），基址回 [4..36]。
2. **状态码值**：规范 VIRTIO_SND_S_OK = **0x8000 段**（实测 resp0=0x8000
   吻合；前轮文档误记 0x8000_0000，已更正）。
3. **枚举下标 + 请求体布局**（SET_PARAMS NOT_SUPP 双因）：
   - formats 位图 0xe0078（bits3-6 连续段）锁死 S16=3（非 2）；速率枚举
     5512..48000 → 48000=7（非 6）；
   - `virtio_snd_pcm_set_params` 参数体 = buffer/period/features +
     **channels/format/rate 三连续字节 @20/21/22**——驱动原把 format/rate
     塞在 padding 区 @26/27，QEMU 读到 format=0(IMAP) 拒绝。

五层全表（round-020/021/022/023）：控制码错段 → DESC_F_NEXT 缺失 →
响应头双字段误判 → 枚举下标错 → 请求体字段错位。

## 改动点

`kunxcore/src/virtio_snd.rs`：状态常量 0x8000 段；ctl_send 状态取 [0..4]；
pcm_info 载荷 [4..36]（resp_len 40→36）；FMT_IDX_S16 2→3、RATE_IDX_48000
6→7、FMT_S16/RATE_48000 检查位同步；set_params 请求体 channels/format/
rate @20/21/22 连续布局。

## 验证

- 构建 0 错误；verbose 启动：init 全链 OK、启动提示音播放、QEMU 告警 0、
  PANIC 0（诊断 dump 保留，kinfo 门控仅 verbose 可见）。
- `make test-audio`：**PASS**（架构切换后首次）。
- 采样率/格式与 audio 核心原有 48k/S16/立体声配置一致，无回退路径变化。

## 遗留问题 / 下一轮建议（按优先级）

1. GUI 正常路径（非 verbose）带音频冒烟 + audiod 服务全链（提示音→
   混音→焦点）在桌面会话中的实听验证（test-audio 已覆盖无头全链）。
2. 转场混合 ~10ms/帧（round-019 遗留，SWAR/降采样）。
3. EV 污染源监控 / syscall 微基准 / round-009 增益复核 / A1（长期）。
