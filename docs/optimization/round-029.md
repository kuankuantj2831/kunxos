# 优化轮 029：ELF 装载段边界共享页根因修复——bench 首跑达成，首个 syscall 基线诞生

- 日期：2026-10-19
- 轮次性质：自动化第 28 轮
- 类型：内核稳定性根因修复（round-028 金矿证据的收网）

## 根因（readelf 铁证 + 全症状闭环）

busybox（LLD 链接）的数据段起始 **0x400876 落在文本页 0x400000 中间**
（未页对齐）。装载器对每个 PT_LOAD 独立逐页 add_page，共享边界页被
映射两次：
1. 第二次 `map_page_in` 用数据段 PTE（RW 无 X）**覆盖**文本段映射 →
   入口取指失败而 `mapped=1`（round-028 线索②闭环）；
2. 第二次分配的新帧清零后只拷了数据段内容 → **抹掉同页文本字节**；
3. 记录表追加**同 VA 双记录** → 退出双重释放（线索③ dbl-track 闭环），
   双释放页回归分配器再利用 → **旁路污染任意进程内存**——round-017
   OOBE EV_LEN 离奇越界的嫌疑机理（同族强关联，未单独定证）。

echo 存活 / bench 必崩的差异：整块/逐页分配路径的时序选择（竞争敏感）。

## 修复（kunxcore/src/elf.rs + vm.rs）

`ProcPageTable::find_mapped_page(vaddr)`（非 LAZY 既有登记查询）+
`set_page_flags`；装载器两条路径（整块/逐页）在 add_page 前检测共享页：
**本段文件内容并入既有物理帧**（按页内偏移）、**权限取并集**（两段各自
过 W^X 校验，共享页须同时满足两段访问权，Linux 同款语义）、PTE 按并集
重映射、记录原位更新、新分配帧归还（块路径归还该页、逐页路径免分配）。

## 验证

| 项 | 结果 |
| --- | --- |
| `busybox bench`（r26 起三次尝试的**首跑**） | ✅ 见下基线 |
| SIGSEGV / dbl-track | **0 / 0**（r28 崩溃配方场景） |
| `busybox echo` / 既有 applet | ✅ |
| lifecycle-core | 38/38（首跑 37/38 为单次时序抖动，复跑全绿） |
| GUI 冒烟（带音频） | 0 PANIC，OOBE 正常，空闲 ~150fps |

### 首个 syscall 基线（QEMU TCG，verbose shell，n=20k/100k 两档稳定）

| 操作 | ns/op |
| --- | --- |
| getpid（陷阱+分发最小路径） | **3,543-3,618** |
| clock_gettime(MONOTONIC) | 30,640-31,677 |
| open+close "/"（VFS 往返） | 31,440-35,883 |

## 遗留问题 / 下一轮建议（按优先级）

1. syscall 优化方向初判：getpid 3.5µs 是陷阱地板；clock_gettime 31µs
   偏重（timer 读路径可剖析——mono_us 41-50µs 的既有观测与此吻合）；
   open+close 31µs 含两次 VFS 锁+路径解析，可接受。
2. round-017 EV 悬案：共享页双释放→页复用污染机理已定位，OOBE 防护
   标记继续监控（若不再复发即告结案）。
3. 转场余档 / boot_trace tcp / A1（长期）。
