# 优化轮 001：memcpy 4 字节前置对齐宽通道（奇 x0 脏区行拷贝 5.35x）

- 日期：2026-10-18
- 轮次性质：手动第 1 轮（每 30 分钟生产自动化 `每30分钟生产任务：kunxos 持续优化轮`
  设立当日跑的第 1 轮；后续自动轮按本目录 round-*.md 最大编号 +1 续编）
- 类型：性能（渲染热路径）+ 测试基建

## 本轮目标

1. 建立 `docs/optimization/` 持续优化轮文档机制（本文件）。
2. 修复内核/用户态 memcpy 宽通道的一个对齐盲区并量化收益。

## 问题

内核 `memcpy`（kunxcore/src/main.rs）与用户态 `wide_copy`
（shared/ttf_text.rs，launcher 每帧 ~4MB 背景恢复+flip 走此）的 `ld/sd`
宽通道只在**两端都 8 字节对齐**时启用，否则整段退化为纯字节循环。

GUI 脏区行拷贝（compositor `copy_rect_pixels`，fb_present 阶段 2
canvas→back）行首地址 = base + (y*stride + x0)*4：stride=2160（8 对齐），
**x0 为奇数像素时行首 ≡4 (mod 8)**，src/dst 同形态——整行退化为字节循环。
光标 26×26 脏矩形、奇 x 坐标的局部更新（时钟/通知/拖动）正是此形态。

## 改动点

| 文件 | 改动 |
| --- | --- |
| `kunxcore/src/main.rs` | `memcpy` 增加「双端 ≡4 (mod 8)」前置 4 字节字拷贝，两端随即同时 8 对齐进入既有 16B/迭代宽通道 |
| `shared/ttf_text.rs` | `wide_copy` 同款前置对齐（volatile 访问，沿用本文件 LoopIdiomRecognize 防护惯例） |
| `kunxcore/src/compositor.rs` | 新增 `pub fn bench_row_copy(x0, w, h, iters)`：同一矩形指定 x0 下 canvas→back 纯拷贝平均耗时 |
| `kunxcore/src/anim.rs` | `perf_test` 新增场景 D：320×200 矩形偶/奇 x0 各 50 次纯拷贝耗时与比值，永久监控对齐路径（`-append "perftest"` 可复跑） |
| `tools/vnc_shot.py` | 新增：零依赖 VNC(RFB) 抓帧工具（QEMU `-vnc :0` → PNG），GUI 冒烟「串口 0 PANIC ≠ 画面正常」的落地手段 |

## 验证

### 构建

`touch kunxcore/ramdisk.img && make kernel`：0 错误（shared 源变化触发全部
应用重编 + ramdisk 重打包，时间戳链 launcher→ramdisk→kernel.img 核对无误）。

### 量化（perftest 场景 D，QEMU virt TCG 540×960）

方法：git stash 暂存 memcpy 改动重建「修复前」内核与当前「修复后」内核各跑
一次 `-append "perftest"`（场景 D 为本轮新增，两边同款）：

| 内核 | even-x0 | odd-x0 | odd/even |
| --- | --- | --- | --- |
| 修复前 | 131µs | **669µs** | **510%**（纯字节循环） |
| 修复后 | 125µs | **125µs** | **100%** |

奇 x0 行拷贝 **669µs → 125µs（5.35x）**，且与偶 x0 完全持平。

场景 A/B/C 宏观数字 4 次运行在 ±5% 噪声带内浮动（A render avg
853/882/863/869µs；B 842/883/867/847µs）——本改动未改变其主导成本（见
遗留问题 1），无回归。

### GUI 冒烟

`-vnc :0` 启动 GUI 实例：串口 0 PANIC；OOBE 向导 30s 无输入自动推进至
第 4/13 步，[perf] overlay 持续统计成千上万帧合成无一异常；VNC 抓帧
（tools/vnc_shot.py，544×960，右缘 4 列为 VNC 表面 544 对齐补边）像素
统计：708 种颜色、白/浅色 UI 背景主导（top5 占 ~46%）、行颜色多样性
2↔47 结构化交替（空白行↔文本行）——画面完好非花屏。

## 遗留问题 / 下一轮建议（按优先级）

1. **【高收益】fb_present 整屏级成本主导**：场景 A（323×200 ≈ 13% 屏）
   render avg ≈ 场景 B（整屏）≈ 860µs，说明 present 路径存在与脏区面积
   无关的整屏级成本（阶段 3 合成/页翻转/ramfb 回写）。下一轮剖析
   `fb_present`/`present()` 计时分解，找出整屏拷贝点并评估脏区化——
   预期是比本轮大一个量级的收益。
2. **memmove 重叠路径**（main.rs / ttf_text.rs）：dst>src 重叠时为反向
   **逐字节** volatile 循环；滚动/窗口拖动若有大块重叠搬移走此路径，
   可改反向字宽通道（保持保序语义）。
3. **memcpy 混合对齐形态**（一端 ≡0 一端 ≡4）仍未覆盖，需移位合并
   （寄存器跨字拼接），收益中等，优先级低于 1/2。
4. **OOBE 帧率复核**：本轮 GUI 实例观测帧间隔 avg ~20ms（≈50fps），与
   此前记录的 81fps 有差距；需复核测量语境（30s 无输入自动推进模式的
   节奏 vs pump 驱动实测）再判断是否回归。
5. **kos/src/error.rs:143** unused variable `per` 构建警告清零。
6. A1 fb EINVAL 遗留稳定性问题：继续待自然复现（BUG_REPORT 档案）。

## 环境坑（沿袭记录）

- rustup 代理坑再现：直接调 `cargo` 报 `unknown proxy name`——统一经
  `make`（PATH 前插 ~/.cargo/bin）构建，勿绕过 Makefile。
- perftest 需 `-device ramfb`，否则「合成器未就绪，跳过测试」。
