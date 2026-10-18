# 优化轮 027：「launcher 开机崩溃环」重定性——fb0 缺失的干净退出重启环（设计内降级）+ SIGSEGV 疑似罕见污染族

- 日期：2026-10-18
- 轮次性质：自动化第 26 轮
- 类型：稳定性定性（round-026 头号发现复核）

## 复核结论（verbose 实测推翻 round-026 的部分解读）

1. **「桌面启动器反复崩溃」重定性**：verbose shell 模式完整启动实测
   **零 SIGSEGV**——真实链路是：shell 模式无 ramfb → `[launcher] open
   /dev/fb0 失败` → launcher **干净退出** → init 按监督策略重启 → 数次
   后 crash-screen 判「反复崩溃」降级控制台。**这是设计内的无显示器
   降级路径**，只是 crash-screen 文案把「退出重启环」说成了「崩溃」，
   以及 launcher 无 fb0 时缺少退避（建议：指数退避或静默等待显示器，
   文案区分 exit/crash——launcher 属并发会话领地，仅列建议）。
   GUI 启动（round-24/25）本就正常，此环不阻塞任何已绿门槛。
2. **真正的 SIGSEGV 仍是谜**（quiet 快速启动偶发、verbose 未复现）：
   pc=0x400000 无甄别力（**所有用户 ELF 都链接在 0x400000 入口**——
   verbose execve-diag 实证），tombstone 的 name=busybox 可信度存疑。
   结合 round-017 OOBE EV_LEN 离奇越界（同属罕见、不可复现、静态审计
   无罪），**疑似同一族「罕见旁路内存污染」**：双 sighting 留档布控
   （OOBE 侧防护标记 + 本轮定性档案）。

## 环境规矩更新（沉淀）

- **诊断一律 verbose**（`-append "verbose shell"`）：本轮实测 verbose
  全程零偶发、且 execve/fork/pf-diag 全量可见——quiet 快速启动的偶发
  SIGSEGV 使其不可作诊断基座的结论进一步坐实。

## 验证

- verbose shell 启动：0 SIGSEGV、fb0 失败→重启环→降级全链清晰可见；
  `cat /data/crash/*` 需动态文件名（tombstone 名含随机数，静态管道
  不可达——本轮教训）。
- execve-diag 实证入口统一 0x400000；fork/perf 统计正常。

## 遗留问题 / 下一轮建议（按优先级）

1. **bench 复活 + 首个 syscall 基线**（verbose 基座已验证可用，代码
   在 round-026 留存——解除阻塞的主交付）。
2. 罕见污染族布控（round-017 防护标记监控 + 本轮档案；第三次 sighting
   时按三案并查：.bss 邻接写、execve 装载竞态、fork COW）。
3. launcher 无 fb0 退避 + crash-screen 文案区分 exit/crash（并发会话
   领地，建议转达）。
4. tombstone 文件名随机化使外部读取困难（cat 需 glob/固定名——低优先
   工具改进）。
5. 转场余档 / boot_trace tcp / round-009 复核 / A1（长期）。
