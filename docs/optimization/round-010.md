# 优化轮 010：kos 警告清零 + lifecycle 门槛纳入 burst + 内存原语改动的存储门槛补验

- 日期：2026-10-18
- 轮次性质：自动化第 9 轮
- 类型：构建质量 + 回归门槛完善 + 验证债偿还

## 本轮目标

1. round-001 遗留的 kos 构建警告清零。
2. round-006 遗留的 `make lifecycle-test` 门槛纳入 burst 脚本。
3. **验证债**：round-001/007/008 重写了 memcpy/memmove——SFS/KOS 块 I/O
   全走这些原语，但并发/信箱/KOS 拷贝门槛从未在新原语上跑过，本轮补齐。

## 改动点

| 文件 | 改动 |
| --- | --- |
| `kos/src/store/mod.rs` | 删除编译器证实的死代码：`write_header_block` 包装、`rewrite_chunks`（内含赋值未读 `buf` 警告）、`extents_block_no`；`commit_record_switch` 未用参数 `root`→`_root`；`freed_blocks` 改先声明后赋值（初值未读）；`leaves` 去 mut；未用 `per` 删除；`FlushPolicy` 两个 variant 补文档 |
| `kos/src/error.rs` | `UploadIncomplete` 字段补文档（missing_docs） |
| `kos/src/store/persist.rs` | `MountHint.magic`/`SIZE`/`to_bytes`/`write_to_sb` 补文档 |
| `Makefile` | `lifecycle-test` 门槛追加 `lifecycle_cmd_burst.py`（完整回归 + 背靠背连发两项都绿才算过） |

## 验证

### 警告清零

`make kosdisk`（kos lib + kosctl 全链）：**0 警告**（修复前 9 条：1 unused_mut、
1 unused variable、2 赋值未读、3 dead_code、5 missing_docs——跨 warning 分组）。
死代码删除由 dead_code 警告证实无任何引用，安全。

### 存储门槛（新内存原语上首次全量补验）

```
make test-sfsrace → SFS RACE TEST: PASS（SFS-RACE + MAILBOX 双判定门）
make test-kos-copy → KOS COPY TEST: PASS（SFS→KOS / KOS→SFS / KOS→KOS 三向 sha1 一致）
```

两门槛均自动重建内核后运行——round-1/7/8 的 memcpy/memmove 重写与 kos
死代码删除在 SFS 并发、rename 原子信箱、KOS 引擎哈希/块路径上全部验证
通过；这是本轮最重要的偿债（此前 9 轮的内存原语改动从未碰过存储回归）。

## 遗留问题 / 下一轮建议（按优先级）

1. **转场混合深水区**（round-009 遗留，26fps → 目标 50+）：候选 u64 双像素
   打包 / 隔行混合 / 转场期动态分辨率（perf-baseline §5.2），需配像素级
   自检。建议下轮整轮投入。
2. A1 fb EINVAL 待自然复现（BUG_REPORT 档案）。
3. crashme 测试应用自身的 unused/dead 警告（故意保留的脚手架，评估加
   `#[allow]` 还是清理）。
4. 应用链接期 `unstable target-feature f/d` 警告（全应用交叉编译统一
   配置问题，一次性修 linker flags）。
5. 混合对齐字内移位合并逼近宽通道（round-008 遗留，收益空间小）。
