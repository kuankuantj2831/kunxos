# KOS 对象存储——架构、用法与故障模型

> KOS（KunxOS Object Storage）是 KUNXOS 的对象存储层：内容寻址 + Merkle
> 校验 + 双池模型（不可变对象池 / 可变记录池）。本文面向使用与排障；
> 设计全貌见 `kos/DESIGN.md`，引擎源码 `kos/src/`，内核集成
> `kunxcore/src/kos_vfs.rs`。

## 一、快速使用

### 应用零改造：`/kos/` 路径

任何应用用**普通 POSIX 系统调用**（openat/read/write/lseek/close）访问
`/kos/<对象路径>` 即可，内核自动路由到对象卷（`kosdemo` 即全程 POSIX）：

```c
fd = open("/kos/demo.txt", O_WRONLY|O_CREAT|O_TRUNC);
write(fd, "hello", 5);
```

- fd 号从 1000 起（避开进程 fd 表），**每进程上限 64**，超出返回错误、
  全关后可再开（`kosdemo` 阶段 5 回归）；
- 进程退出（含被强杀后收尸）自动回收其全部 KOS fd——无泄漏、pid 复用
  拿不到死者 fd。

### shell 命令族

```
kos ls [path]            列目录
kos cat <path>           读对象
kos stat / kos df        卷状态（generation/对象数/空闲块）
kos du <path>            对象大小与类型
kos mkdir <path>         建目录（幂等）
kos write <path> <text>  写可变记录
kos append <path> <text> 追加
kos mv <from> <to>       改名
kos cp <src> <dst>       引擎级复制（单锁零用户态往返）
```

`kos cp` 语义：可变记录 → 16KB 分块流式（任意大小）；不可变对象 →
内容寻址**天然去重**（同内容不占新块）。

### 无缝复制（跨池）

shell `cp` 直接跨 SFS ↔ KOS，无需任何前缀命令：

```
cp /bin/hello /kos/data/from-sfs.txt     # SFS → KOS
cp /kos/data/x /bin/back.txt             # KOS → SFS
cp /kos/data/x /kos/data/y               # KOS → KOS
cp /bin/hello /kos/data/                 # 复制进目录（自动拼 basename）
```

回归：`make test-kos-copy`（QEMU 四哈希判定，幂等可重复跑）。

### 构建与磁盘

```
make kos-disk     # 生成 256MB 持久卷 kos-disk.img（含签名信任锚）
make run          # 带卷启动（QEMU）；启动日志见 "[kos] 持久对象存储卷已挂载"
run /bin/kostest  # 内核侧全功能验收（含故障降级阶段）
run /bin/kosdemo  # 纯 POSIX 桥验收（含 fd 上限阶段）
```

## 二、内核接口

| 系统调用 | 号 | 说明 |
|---|---|---|
| kos_mount/read/write/mkdir/unlink/list/stat | 460-466 | 基础面 |
| kos_append / truncate / list2 / read_range | 467-470 | 可变记录 |
| kos_rename / open / read_fd / close_fd / write_range | 471-475 | fd 层 |
| kos_up_begin/push/status/finish | 476-479 | 断点续传 |
| **kos_fault** | **480** | root 专用 I/O 故障注入开关 |

错误统一经 `kos_vfs::kos_errno` 映射为负 errno（ENOENT/EACCES/EIO/
EEXIST/ENOSPC…），**任何失败都不会 panic 内核**。

## 三、故障模型（「报错了还能继续运行」）

1. **I/O 故障降级**：块设备读写失败 → 该操作返回 `EIO`；首个失败经
   health 报「卷已降级（系统继续运行）」，恢复后自动报恢复并继续服务。
   验证：`kosctl 480` 注入 + `kostest` FAULT-DEGRADE 阶段（QEMU）；
   宿主故障注入矩阵 `kos/tests/fault.rs`（全 API 零 panic + 内容无脏态）。
2. **断电一致性**：数据块 CoW 先行、头块是唯一提交点、manifest 双槽
   generation 择优。任意写序断点后重挂载：内容 = 旧值或新值，绝无第三
   态（`tests/crash.rs` 全断点矩阵背书）。
3. **快速挂载与回退**：持久索引（位图+对象索引链+超级块 hint）秒级挂载
   大卷；hint 过期 / 位图块数不符 / 任一读失败 / 根不可达等**九重防护**
   任一命中即回退权威全池扫描——快速路径永不产出错误状态。
4. **超级块损坏自愈**：布局对同一容量是纯函数，块 0 打花后挂载直接重建
   （hint 随之失效，自动走扫描）。
5. **磁盘格式全 CRC**：对象头/manifest/命名空间节点/Merkle 索引块/区段
   表/超级块全覆盖（注意：**所有字段必须先于 CRC 写入**——曾因 reserved
   后写导致 mtime 非零对象重挂载必败，2026-10-18 修复，四序列化器统一）。

## 四、与 SFS 的关系

双跑并存（Phase 1 设计）：SFS 仍是根文件系统；KOS 挂在第二块盘（块 0
magic `KOSV0001` 自动探测，SFS 盘 magic 不同无冲突；无盘时回退内嵌 RAM
卷）。两者**共用同一把 fs_lock 大锁**串行化，无锁序问题。`/kos/` 前缀在
`vfs::open/read/write/close/lseek/stat/lstat` 统一路由。

## 五、测试地图

| 套件 | 覆盖 |
|---|---|
| `kos/tests/{store,mutable,format_roundtrip,merkle,tamper,fuzz,capacity}.rs` | 引擎基础/篡改/模糊/2TB 容量 |
| `kos/tests/crash.rs` | 全断点断电矩阵 + 断点续传跨断电 |
| `kos/tests/fault.rs` | 运行期 I/O 故障注入（瞬时/永久） |
| `kos/tests/upload_gc.rs` | 会话取消/超时回收/finish 重挂载 |
| `make test-sfsrace` / `test-mailbox` | 内核回归（判定门含 MAILBOX） |
| `make test-kos-copy` | 三向无缝复制 + 引擎级 cp（QEMU） |
| `run /bin/kostest` / `/bin/kosdemo` | QEMU 全功能 / POSIX 桥 + fd 上限 |

宿主跑法（本机 rustup 代理有坑，必须前插工具链 bin）：

```
cd kos && export PATH="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH" \
  && CARGO_TARGET_DIR=$PWD/target cargo test
```
