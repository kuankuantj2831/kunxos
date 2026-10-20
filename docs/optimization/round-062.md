# 优化轮 062：mailbox 诊断包首捕——读者 open 自己刚 rename 的文件失败，矛头指向 rename 可见性竞态

- 日期：2026-10-21
- 轮次性质：自动化第 61 轮（r61 #1 诊断包实施）
- 类型：可观测性 + 根因线索突破

## 诊断包（kunxcore/src/vfs.rs open_impl）

O_CREAT 分支失败时 kinfo 打印 `[mbx-diag] create 失败 path= free_ino= dir=`
（区分卷 inode 耗尽 vs 目录项失败）+ `resolve_parent 失败`（中间组件
消失形态）。

## 首捕（本轮 armed 运行，败局现场）

```
[vfs] open 失败 path=/var/run/mbx.pipe      err=-1 used=8/256   ← 写者 O_CREAT
[vfs] open 失败 path=/var/run/mbx.pipe.r_380 err=-1 used=8/256  ← ★读者 O_RDONLY！
[sfsrace] MAILBOX FAIL: 丢失消息 W0 seq=7
（mbx-diag 两行均未触发——失败都在非 O_CREAT 路径）
```

## ★ 根因线索质变

**读者 rename(pipe→taken_380) 刚返回成功，紧接着 open(taken_380, RDONLY)
就 ENOENT**——不是写者问题、不是 inode 耗尽（used=8/256、r58 回收已证
干净），是 **rename 后新名的可见性竞态**：

- 候选 A：SFS journal（jlog）提交时序——rename 的事务尚未落盘可见，
  后续 fs_lookup 读到旧目录块 → ENOENT → 读者「打开认领文件失败」
  退出/跳过 → 该文件内全部消息未消费 → 终局「丢失消息」✓ 签名吻合；
- 候选 B：dcache 负缓存（无——只存正向）；fs_rename 双 bump 本身无窗。

这是 r39 以来首次把 mailbox 悬案钉到**单一内核机制**上。写者
open(pipe) 失败同源：rename 走后旧名 pipe 的目录项删除若同样延迟，
并发 O_CREAT 与残留项冲突。

## 下轮定证（一步之遥）

`fs_simple::fs_rename` 的 jlog 提交路径：事务 commit 与目录块回写是否
同步完成？若是异步/延迟，`fs_lookup` 在提交窗口内读旧块 = 复现条件。
修复方向：rename（及 unlink/create）事务内同步回写目录块，或 lookup
走 jlog 现势视图。

## 遗留问题 / 下一轮建议（按优先级）

1. **fs_rename/jlog 可见性定证**（上节；mailbox 悬案收官最后一步）。
2. EV 监控；A1。
