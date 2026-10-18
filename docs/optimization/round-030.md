# 优化轮 030：clock_gettime 提速 26%（归一化）——砍 user_ok_writable 冗余页表遍历 + 合并用户写

- 日期：2026-10-19
- 轮次性质：自动化第 29 轮
- 类型：syscall 性能（round-029 基线指认的头号方向）

## 成本剖析（按 round-029 基线追凶）

内核侧 `now_us()` 本已极致（`csrr time` 单指令 + 纯算术，无 SBI/MMIO）。
31µs 的大头在 **`user_ok_writable` 的三到四轮线性记录表扫描**：
`ensure_user_range_writable` 内部每页做 user_range_mapped（记录扫描 +
必要时 PTE 漫游）+ current_page_flags（又一轮扫描），成功后
`user_ok_writable` **再跑一遍全量 user_range_mapped——纯冗余**（ensure
成功已蕴含「已映射且可写」，含 SHM 共享映射的 PTE 放行路径）。记录表
大的进程（OOBE/launcher 500+ 页）线性扫描更甚——**mono_us 41-50µs 的
既有观测与此同源**。

## 改动点（kunxcore/src/linux_compat.rs）

| 位置 | 改动 |
| --- | --- |
| `user_ok_writable` | 删除尾部冗余 `vm::user_range_mapped` 第二轮全量遍历（ensure 语义已覆盖；全体可写校验 syscall 受益） |
| `sys_clock_gettime` | 两次 `write_u64_to_user`（各带一对 PAN 放行开关）合并为单次放行 16B 直写 |

## 验证

### bench 前后（跨启动，getpid 作宿主漂移对照）

| 操作 | 前（r29） | 后（r30） | 归一化（÷getpid） |
| --- | --- | --- | --- |
| getpid（对照） | 3,543-3,618 | 4,063（宿主 +13%） | 1x 基准 |
| clock_gettime | 30,640-31,677 | **26,452** | 8.8x → **6.5x**（**-26%**） |
| open+close | 31,440-35,883 | 39,119 | 同受漂移影响，未归因 |

### 回归

lifecycle-core **38/38**（时钟/可写校验路径全覆盖）；GUI 带音频冒烟
0 PANIC、OOBE 正常。

## 遗留问题 / 下一轮建议（按优先级）

1. **记录表辅助索引**（结构性）：`root_page_recorded`/`current_page_flags`
   均为 O(count) 线性扫描，页数大的进程每次校验都付全额——加 per-process
   页记录哈希/基数树索引，clock_gettime 与全部 copy_to/from_user 受益，
   预期再砍一半以上。
2. mono_us 用户态观测复核（记录表索引落地后对比 41-50µs 基线）。
3. EV 悬案监控 / 转场余档 / boot_trace tcp / A1（长期）。
