# 优化轮 036：dentry 缓存——深路径 open+close 126→80µs（-37%），四点代际失效

- 日期：2026-10-20
- 轮次性质：自动化第 35 轮
- 类型：syscall/存储性能（round-035 立项施工）

## 实现

| 文件 | 改动 |
| --- | --- |
| `kunxcore/src/vfs.rs` | dentry 正向缓存：(dev, 父 inode, 名字≤28B)→子 inode，环形 64 槽 + **全局代际**；`walk_from` 每组件先查缓存，miss 走 `fsops::fs_lookup` 后回填 |
| `kunxcore/src/fsops.rs` | 四个目录变更分发点（fs_create/fs_mkdir/fs_unlink/fs_rename）入口统一 `dcache_invalidate()` bump 代际——分发层全集即失效全集 |

设计说明：代际方案把失效边收敛到分发层 4 处（而非各文件系统的
N 处内部写点）；漏 bump 后果=陈旧命中错 inode，以 sfsrace/mailbox
（并发创建/删除/rename 压测）+ kos-copy 三向一致性为正确性门。

## 量化（bench n=100k，同宿主对照 getpid 2.5µs 稳定）

| 操作 | r35 | r36 | Δ |
| --- | --- | --- | --- |
| open+close 深路径（33 字符 ×6 级） | 126,168 | **79,907 ns** | **-37%** |
| open+close "/" | 18,943 | 19,066 | 持平（单组件根路径缓存收益小） |

## 回归（失效正确性关键门）

- **SFS RACE + MAILBOX：PASS**（6 进程并发创建/写/截断/删除 + rename
  原子信箱——目录变更密集场景，缓存失效正确性的直接压力测试）
- KOS 三向拷贝 sha1 一致 **PASS**；lifecycle-core **38/38**。

## 遗留问题 / 下一轮建议（按优先级）

1. **余下 80µs 拆解**：每组件两次未缓存 `fs_type`（inode 读）——dentry
   表增存子类型 + walk 线程传递 cur_type，可免两读/组件（预期再 -50%，
   80→~35µs）；末分量后的 open 本体 inode 读另计。
2. inode 表 LRU 缓存（结构性，覆盖 fs_type/fs_read 元数据全家族）。
3. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
