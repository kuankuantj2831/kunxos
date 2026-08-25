# PC=0 / 稳定性调查笔记（最终）

## 结论

1. **内核 PANIC（memcpy/memset dst=2）是探针诱导的**：热路径（irq epilogue /
   syscall restore）插入 last-eret 存储改变 IRQ 返回时序 → 每轮必 PANIC。
   **移除全部热路径探针后 100+ 轮 0 PANIC**。教训：热路径禁止加任何指令。

2. **残余 el0_fault（PC=0，~0-15%，时序敏感波动）**：
   - 帧 elr=0、ESR EC=0x20（指令中止）、用户 x30=0、x8=64（write）、
     sp_el0 靠近栈顶（新进程）——发生在 fork+exec 进程的首次 write 附近
   - 并行代理已修复 kunxinit spawn_service 的栈局部缓冲越界（用户侧根因之一），
     残余率略降但未消除
   - 内核侧所有 eret 路径已校验（syscall restore 检查 + irq epilogue 新检查），
     未发现可 eret 到 0 的路径；x30=0 暗示用户侧 ret/br 到 0 或未明竞态
   - 已隔离：进程被杀，内核 0 PANIC 稳定
   - 一个变体：未定义指令 @ 0x400a958c（1GB+ 内核文本区）——EL0 取指内核
     文本按 AP_EL1_RO 应被拒（权限故障），实测为 EC=0x02 未定义指令，机制
     未完全闭合（用户页表 inherit_kernel_mapping 继承 L1[1] RAM，AP 保护生效，
     内核内存不可被 EL0 访问）

## 已实施修复（本轮，保留）

- **sync_entry 入口 `msr daifset, #2`**：防 IRQ 在异常入口窗口覆盖 elr/spsr
- **irq_entry 入口 `msr daifset, #2`**：显式化硬件 I=1
- **irq epilogue ELR 校验**（纵深防御）：返回 EL0 前检查 elr_el1，0 或 ≥1GB
  时把 IRQ 帧转成陷阱帧走 el0_fault；校验在 pop 之前不污染用户寄存器；
  EL1 返回跳过。实测多轮 0 panic 无误伤
- **SIGKILL 投递**：dispatch 入口检查信号位图，SIGKILL 置位即终止进程
- **shell `kill` / `sleep` 命令**（信号投递 + 单调时钟睡眠）
- **virtio_input 事件时间戳**：tv_sec/tv_usec 填内核单调时钟（原为 0）
- **networkd 管道缓冲 256→1024**：长命令不再截断丢失

## 保留诊断（冷路径）

- PANIC_REGS（sync_entry 汇编保存故障寄存器）+ panic handler [regs]/[ret-scan]/[stack]
- el0_fault_handler 的 [el0-fault frame] + [user-bt] + [stack-canary]（并行代理）

## 教训

- 热路径任何新增指令都会移动时序竞态窗口——调试探针只能放冷路径
- 异常入口诊断要先 daifset，否则 elr/spsr/寄存器被 IRQ 污染
- el0_fault 的帧寄存器组合（x8=64 + x30=0 + 新进程 + 首次 write）是定位
  PC=0 的关键指纹

## 追加结论（第 10 轮）：残余 el0_fault 根因 = 用户态应用缓冲区溢出

- **证据**（el0_fault_handler 的 [user-stack] 转储，并行代理所加）：
  - run-10：故障进程 pid=4（launcher），用户栈第 21 字 = 0x3c23d70a
    （堆区指针），进程 PC 最终 = 0x3c23d70a（ESR=0x8a000000 EC=0x22
    PC 未对齐）——**返回地址槽被堆指针覆盖 → ret 到垃圾地址**
  - run-1：栈上出现 "launcher\0" 字符串 + 大量代码/堆指针 + 0x3c23d70a
  - PC=0 变体（EC=0x20）= 同一机制：保存的 LR 槽被清零 → ret 到 0
- **结论**：故障进程全是复杂 GUI 应用（launcher/oobe/displayd），用户侧
  缓冲区溢出覆盖栈帧保存的 LR → ret 到 0/垃圾 → el0_fault。内核侧所有
  eret 路径已校验（无内核 eret 到 0 的路径），copy_from/to_user 有 user_ok
  + 页映射双重校验——内核正确隔离了用户态 bug
- **无需内核修复**：若要消除故障需审计应用（apps/launcher、oobe）的
  framebuffer/input 处理代码（大工作量，非内核缺陷）
