# 优化轮 007：memmove 反向宽通道——重叠拷贝（滚动/平移形态）10.4x

- 日期：2026-10-18
- 轮次性质：自动化第 6 轮
- 类型：性能（内存原语热路径，round-001 遗留项）

## 本轮目标

memmove 的 dst>src 重叠路径（滚动、同缓冲平移的形态）此前是**反向逐字节
volatile 循环**（内核 kunxcore/src/main.rs 与用户态 shared/ttf_text.rs 同款），
256KB 级块搬移在 TCG 下不足以喂满帧预算。改写为与 memcpy 同款的反向宽通道。

## 改动点

| 文件 | 改动 |
| --- | --- |
| `kunxcore/src/main.rs` `memmove` | 重叠分支重写：两端 8 字节**同余**时先回拷尾部字节对齐（同时落 0 mod 8），随后 16B/迭代反向宽通道（`ld/sd` 负偏移寻址）+ 8B 收尾；余数不同余（无法互相对齐）或头部剩余回退字节 volatile 循环。`n<32` 直接字节循环（小拷贝不值得对齐开销） |
| `shared/ttf_text.rs` `memmove` | 用户态同款重写（volatile 防护保留，防 LoopIdiomRecognize 改写回 `call memmove` 递归） |
| `kunxcore/src/compositor.rs` | 新增 `bench_overlap_copy(h, iters)`：专用 kmalloc 草稿缓冲上做 320×h 块下移 16 字节（dst>src 全程重叠）计时；**附单次语义正确性抽查**（序号模式重填 → 单次搬移 → 逐字核对），错序返回哨兵值 |
| `kunxcore/src/anim.rs` | perftest 新增场景 E：输出重叠拷贝 µs/次或「正确性失败」 |

对齐正确性论证：反向拷贝从高端向低端推进，先对齐**尾端**（与 memcpy 对齐
首端互为镜像）；两端同余保证减去同样字节数后同时 8 对齐；宽通道每步 16B
递减不破坏对齐；dst>src 时高端先行，源数据在被覆盖前已被读取（保序语义
与逐字节反向完全一致）。

## 验证

### 量化（perftest 场景 E，QEMU virt TCG，git stash 对照）

| 内核 | E/overlap-copy-320×200+16B | 正确性抽查 |
| --- | --- | --- |
| 修复前（反向字节循环） | **663µs/次** | —（无抽查） |
| 修复后（反向宽通道） | **64µs/次** | 通过（逐字核对） |

**10.4x**。场景 A/B/C 同场无回归（A fps 2439、B render avg 876µs、
C fps 1461，均在近几轮噪声带内）。

### 构建与 GUI 冒烟

`make kernel` 0 错误（shared 变更触发全应用重编；stash 往返两次重建均 0
错误）。GUI 实例：0 PANIC，OOBE 推进至 4/13，空闲动画维持 round-5 水平
（间隔 avg ~9.7ms，尖峰 0），直绘渲染 39µs，抓帧像素统计与历史基线一致
（708 色/行多样性 2-47 结构化）。截图 /tmp/kunxos-vnc-r7.png。

### 过程异常记录（诚实性）

首轮单字 `-append "perftest"` 运行日志缺 anim-perf 输出（boot 至 GUI
正常）；立即复跑同参数 14 行齐全，加 `verbose` 亦正常。判定为偶发（疑似
构建负载干扰串口捕获），未复现，与本改动无关（改动代码路径在 perftest
之前未被触碰的构建期已存在）。

## 遗留问题 / 下一轮建议（按优先级）

1. memcpy 混合对齐形态 (0,4)/(4,0)（round-001 遗留，需寄存器跨字拼接）。
2. OOBE 步骤转场（200ms 淡入淡出）帧率复核（round-005 遗留）。
3. kos/src/error.rs:143 unused variable 警告清零（round-001 遗留）。
4. `make lifecycle-test` 门槛纳入 burst 脚本（round-006 遗留，一行改动）。
5. A1 fb EINVAL 待自然复现（BUG_REPORT 档案）。
6. memmove 小尺寸门槛（n<32）与尾部对齐开销的微调空间（当前保守，未见
   实测瓶颈，仅记录）。
