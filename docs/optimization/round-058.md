# 优化轮 058：SFS 回收判别实验——产品无罪定证，mailbox 阶段止损停车

- 日期：2026-10-21
- 轮次性质：自动化第 57 轮（r57 判别实验执行）
- 类型：定证 + 止损决策

## 判别实验（r57 方案）

shell 模式启动后对 /var/run 执行 **150 次 create/unlink 循环**
（5 个名字轮转，python 管道驱动，逐次判定）：

```
/var/run 目标 open 失败：0/150
rm 错误：0/150
（日志中 22 条 [vfs] open 失败均为开机期对不存在文件的探测噪音：
 /data/wifi、/sfsrace-test 标记、shell 模式的 /dev/fb0 等，非本轮目标）
```

## 判决

- **SFS create-after-unlink 回收正常，产品无罪**——r57 统一假说的
  产品缺陷分支死亡；
- writer open 失败（r57 iter=29）机制仍未定证，但**已完全圈死在
  测试 harness 的信箱阶段**（产品并发阶段 SFS-RACE 历次全 PASS）。

## 止损决策（五轮投入后的停车线）

mailbox 阶段历史上从未稳定（r37 前 ~1/5 抖动，r40 后重载下更差），
已投入 r39/40/56/57/58 五轮。**停车**：
- 门槛使用规约：sfsrace 的 MAILBOX FAIL 视为已知抖动，**单次重跑**
  后仍 FAIL 才判回归（SFS-RACE 阶段 FAIL 无此豁免——那是产品门）；
- 重启条件：出现新证据（如 t2f 轮偶得 iter=29 时的 used=N 快照）或
  专门排期（方案方向：写者 open 失败 errno 打印 + fd/inode 快照）。

## 遗留问题 / 下一轮建议（按优先级）

1. 密码/胁迫页定向输入（boot_trace 再省 ~60s）。
2. 缺页/COW 缓存化（低）；EV 监控；A1。
3. mailbox 诊断包（重启条件触发时：errno+fd 表+inode 计数快照注入）。
