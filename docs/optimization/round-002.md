# 优化轮 002：fb_present prev_app 累积并集根因修复——局部脏区呈现 6.6x、叠加窗口 2.4x、OOBE 直绘 25-40x

- 日期：2026-10-18
- 轮次性质：自动化第 1 轮（每 30 分钟生产任务触发）
- 类型：性能（合成器呈现热路径，兼稳定性：消除状态永久退化）

## 本轮目标

执行 round-001 头号候选：解释并消除「场景 A（≈13% 屏局部脏区）render 耗时
≈ 场景 B（整屏）≈ 860µs」的面积无关整屏级成本。

## 根因分析（先加测量，再定位）

1. 给 `fb_present` 加阶段耗时分解（`STAGE_US`：meta+拷贝 / 合成 /
   present，perftest A/B 输出），实测：A 与 B 的 meta+copy **同为 ~827µs**
   ——拷贝量差 8 倍耗时相同，copy 区域实际是整屏。
2. 追踪数据流：`copy = prev_app ∪ app_dirty ∪ 内核失效区`，而
   `prev_app ← copy`（kunxcore/src/compositor.rs fb_present 阶段 3）——
   **累积并集只增不减**。任何一次整屏事件（窗口销毁、页面切换）后，
   之后每一帧都被永久锁死为整屏拷贝（2MB/帧 canvas→back）。
3. 正确语义（原注释自己写明的「2 帧并集」）：双缓冲轮转下，下一次呈现
   写入的 back 含 2 帧前内容，只需补 `dirty_{N-1} ∪ dirty_N`；更早变更
   已存在于该缓冲。prev_app 应取**本帧新变更**而非历史并集。

## 改动点

| 文件:位置 | 改动 |
| --- | --- |
| `kunxcore/src/compositor.rs` fb_present 阶段 1 | 新增 `newd[]` 累加器：app_dirty 与内核失效区在并入 copy 的同时并入 newd |
| `kunxcore/src/compositor.rs` 阶段 3 | 阶段 2 期间新增的 IRQ 侧失效区同样并入 newd；`prev_app ← newd`（不再取累积 copy） |
| `kunxcore/src/compositor.rs` fb_present | 新增 `STAGE_US`/`last_stage_us()` 阶段耗时分解（保留，供后续剖析） |
| `kunxcore/src/anim.rs` perf_test | 场景 A/B 输出逐阶段 µs/帧分解（保留） |

## 验证

### 构建

`make kernel` 0 错误（kernel.img 15:38:07 重建）。

### perftest 量化（QEMU virt TCG 540×960，-append "perftest"）

| 场景 | 修复前（本轮首测） | 修复后 | 提升 |
| --- | --- | --- | --- |
| A 局部脏区 320×200 render avg | 866µs（fps 859） | **132µs（fps 2531）** | **6.6x** |
| A meta+copy 阶段 | 828µs | 125µs（≈场景 D 单矩形 124µs，即 2 帧并集真实成本） | 6.6x |
| C 叠加窗口 alpha 移动 | fps 627 | **fps 1506**（render avg 1505→674µs） | **2.4x** |
| B 整屏 | render 868µs | 836µs | 持平（整屏本就该全拷，符合预期） |
| D 行拷贝对齐 | 124/124µs | 124/124µs | 不受影响 |

A 首帧 max=971µs 为整屏事件经 2 帧窗口排出的过渡尾巴，随后稳定 ~132µs；
C 的移动叠加区不再随行程增长（旧实现 120 帧后膨胀到全宽条带）。

### GUI 冒烟（-vnc :0 实例 + tools/vnc_shot.py）

- 串口 0 PANIC；OOBE 30s 无输入自动推进观察至 7/13，每步页面切换（整屏级
  脏区）正常，数千次 present 无异常。
- **真实收益**：OOBE 稳态「直绘渲染」从 round-001 同场景实测 avg
  1600-1800µs 降到 **39-65µs（25-40x）**；关中断临界区 avg 41→25µs。
- 抓帧像素统计（两次，相隔 60s）：708/554 种颜色、白/浅色 UI 调色板主导、
  行多样性 2↔47/51 结构化交替、纯黑占比 1%（仅 VNC 544 对齐补边列）——
  无花屏/大面积残影/异常黑区。
- 画面截图：/tmp/kunxos-vnc-r2.png、/tmp/kunxos-vnc-r2b.png。

### 正确性论证（残影风险的说明）

prev_app 语义从「历史并集」收紧为「本帧新变更 ∪ 本帧内核失效」，恰为原
注释声称的 2 帧并集（dirty_{N-1} ∪ dirty_N）；静止覆盖窗口区域在 back
轮转缓冲中已含正确内容（N-1 帧已合成过），不需每帧重拷。perftest C
（移动 alpha 窗口 ×120 帧）render 稳定无漂移、OOBE 页面切换无残影统计
特征。局限：本环境无法人眼审视动画过渡，下轮以 GUI lifecycle-test
（lcapp 启动/后台化/恢复，覆盖窗口动画+应用脏区交错）做端到端复核。

## 遗留问题 / 下一轮建议（按优先级）

1. **GUI lifecycle-test 端到端复核**（`make lifecycle-test`，docs/
   lifecycle-test.md）：本轮改动的窗口动画/应用脏区交错场景回归 + 顺手
   观察桌面（launcher）路径的直绘渲染是否同样降至百 µs 级。
2. **OOBE/应用帧节奏**：渲染已降到 40µs，但帧间隔仍 ~19.7ms（min
   16.5ms ≈ 51fps 上限）——瓶颈在应用主循环节拍（pump/FRAME_INPUT 门），
   与动画期 81fps 记录的差距在节奏侧不在合成器侧，值得单独一轮剖析。
3. memmove 重叠路径反向逐字节循环（round-001 遗留，滚动场景）。
4. memcpy 混合对齐形态 (0,4)/(4,0) 未覆盖（round-001 遗留）。
5. kos/src/error.rs:143 unused variable 警告清零（round-001 遗留）。
6. A1 fb EINVAL 待自然复现（BUG_REPORT 档案）。

## 环境坑（新增）

- 本机 `pkill`/`pgrep` 部分参数被宿主包装影子化（`-9` 报无效选项），
  收 QEMU 用 `kill <pid>` 直杀。
