# 优化轮 037：类型线程尝试——深路径实测 24µs（-81% 两轮累计）但引入偶发 mailbox 丢认领，按红线回滚

- 日期：2026-10-20
- 轮次性质：自动化第 36 轮
- 类型：尝试→发现真 bug→回滚（红线执行轮）+ 坑档案

## 尝试（已回滚，性能数据仍有效）

r36 余量施工：`DCacheEntry` 增存子类型 + `walk_from` 线程传递 `cur_type`
（symlink 跳转后 UNKNOWN 哨兵下轮顶实测），目标免每组件两次 `fs_type`
inode 读。

**性能达成**：深路径 open+close **79.9 → 24.2µs（-70%；两轮累计
126→24µs = -81%）**——余量判断完全正确。

## 为什么回滚：真·偶发正确性 bug

同二进制下专用 `test-sfsrace` 时过时败；lifecycle 启动（泄漏标记触发
的嵌入式 sfsrace）日志实锤失败签名：

```
[mbx-dbg] 超限时 dones=2
[sfsrace] SFS-RACE FAIL (mailbox)
```

**丢 2 条 done 认领**——mail 阶段是并发 rename（tmp→done 认领），失败
签名指向路径解析在并发变更下偶发出错（典型陈旧缓存形态；具体机理
本轮时间盒内未定证——候选：缓存 typ 与 rename 后目标的时序窗、
T_UNKNOWN 分支与绝对 symlink 的交互）。回滚后复测：sfsrace PASS、
lifecycle 38/38，双双恢复。

## 处置

- `git checkout kunxcore/src/vfs.rs` 回滚全部 r37 改动（r36 dcache 完整
  保留，深路径维持 80µs）；干净重建 + 双门复绿。
- 失败签名 + 24µs 性能证据留档：下次重试时先写**并发 rename 压测的
  定向复现**（复现脚本化→二分缓存字段→再上），不许直接重试。

## 附：本轮踩中的已知坑（第二次）

**test-* 标记泄漏**（r24 audio 版同款）：`make test-sfsrace` 后 rootfs
标记虽被 make 清理，但 kernel.img 仍嵌标记时代 ramdisk——后续 lifecycle
两跑全被污染（伪 FAIL）。规矩重申并升级：**凡跑过 test-sfsrace/
test-audio/test-modem，必须 touch ramdisk 重建 + 无头 grep 验证无标记**。

## 遗留问题 / 下一轮建议（按优先级）

1. 类型线程 v2（先并发 rename 定向复现脚本，定证机理再上；性能 prize
   80→24µs 已实证）。
2. inode 元数据 LRU（替代方案：覆盖 fs_type 家族而不动 walk 语义）。
3. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
