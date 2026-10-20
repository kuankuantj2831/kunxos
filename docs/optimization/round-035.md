# 优化轮 035：读侧校验三连修——open+close "/" -24%，并爆出目录逐组件 17µs 新目标

- 日期：2026-10-20
- 轮次性质：自动化第 34 轮
- 类型：syscall 性能（round-034 头号目标 open+close 的第一刀）

## 三连修（读侧镜像 r30/r33）

| 位置 | 改动 |
| --- | --- |
| `linux_compat.rs` `user_ok` | 砍尾部冗余 `vm::user_range_mapped` 第二轮全量遍历（r30 砍了写侧同款，读侧漏网；ensure_user_range_mapped 成功即「已映射」，含共享映射放行） |
| 同 `read_cstr` | 逐字节 user_ok（每字节两轮映射检查，33 字节路径=66 次）→ **逐页校验**（同页免重验；越界语义不变——进入未校验页前必先触发该页校验） |
| `process.rs` `root_page_recorded` | 吃 r33 自证式单槽缓存（与 lookup_page_idx 共享槽位，LAZY 语义在命中分支内正确处理；互相覆盖最坏=退回扫描） |

## 量化（bench n=100k，同日同宿主对照）

| 操作 | r34 | r35 | Δ |
| --- | --- | --- | --- |
| open+close "/" | 24,903 | **18,943 ns** | **-24%** |
| getpid / clock_gettime（对照+红利） | 3,287 / 3,893 | 2,493 / 2,826 | 宿主更快+读路径受益 |

### 新目标爆出：长路径（新增 bench 用例）

```
open+close 长路径(33 字符 /var/lib/pkg/repo/main/Packages) = 126,168 ns
```

**每级目录组件 ~17µs**（≈一次根打开成本 × 6 级 + 文件本体）——路径解析
无 dentry 缓存，每组件都做目录块读 + 线性 dirent 扫描。pkg/store 类
深路径访问全部付此税。**dentry 缓存立项**（下一轮）：热点 (父 inode,
名字) → 子 inode 的 LRU 小缓存即可砍掉大头。

## 回归

lifecycle-core **38/38** + cmd-burst **PASS**（读路径/文件 I/O 重度覆盖）。

## 遗留问题 / 下一轮建议（按优先级）

1. **dentry 缓存**（本轮实证：每组件 ~17µs；LRU (父,名)→inode 小缓存，
   深路径预期 -70%+）。
2. 缺页/COW 查找缓存化（低优先）。
3. EV 监控 / 转场余档 / boot_trace tcp / A1（长期）。
