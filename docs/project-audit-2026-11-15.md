# KunxOS 第二轮补充审计（2026-11-15）

本轮检查当前工作区，沿用上一轮 B01–B24 编号，补充 B25–B33。**确认的 9 项问题现已修复，状态与回归命令见 [修复记录](repair-status-2026-11-15.md)。** 下文保留修复前的审计证据；测试脚本已经改成正向回归，旧的 REPRO 完成标记不再适用。工作区已有上一轮及其他会话的修改，均予保留。本轮不保证发现了全部 bug；重点覆盖向量 I/O、fd 标志、管道端点和同步记录状态转换。

## 需要优先修复的问题

### B25 [P1] readv/writev 会跨过尚未完成的 iovec，破坏字节顺序

位置：`kunxcore/src/linux_compat.rs` 的 `sys_writev` / `sys_readv`，每项只处理 `len.min(4096)`，处理完就进入下一项。

真实 RISC-V 来宾复现：第一项为 8192 个 A，第二项为 4 个 B；writev 返回 4100，文件实际是 **4096 个 A + 4 个 B**。合法短写可以只写前 4096 字节，但不能跳过第一项后 4096 字节再写第二项。若应用按返回计数重试，输出仍会重排/重复。

readv 同样返回 4100，却把末尾 4 个 B 写到第二个目标缓冲区，第一项的 4096..8192 仍为空；返回值不对应输入向量的连续前缀。

建议：逐项逐块完成，遇短读/短写立即返回已完成前缀；不要因内核临时缓冲区大小而跳到下一项。

### B26 [P1] ftruncate 可以通过只读描述符清空文件

位置：`kunxcore/src/vfs.rs` 的 `truncate`；`sys_ftruncate` 直接调用它。只验证槽位与文件类型，没有检查打开访问模式。

来宾复现：写入 4100 字节文件，关闭写描述符，以 O_RDONLY 打开，ftruncate(fd, 0) **返回 0**；随后 SEEK_END 返回 **0**，确认文件真的被清空。这是描述符访问模式缺失，与上一轮普通 VFS 缺少 owner/mode 的 B01 是两个层次的问题。

建议：截断前要求 O_WRONLY/O_RDWR；只读描述符返回 EBADF，并对普通与 KOS 路径保持契约一致。

### B27 [P1] fcntl 的成功桩会让 O_APPEND 被静默忽略，覆盖原文件

位置：`kunxcore/src/linux_compat.rs` 的 `sys_fcntl`。F_DUPFD 直接走 dup，忽略最小 fd；其余命令一律返回 0。

来宾复现了四种表现：

- F_DUPFD(fd, 20) 返回小于 20 的描述符。
- O_RDWR 描述符执行 F_GETFL 得到 0，错误报告为只读访问模式。
- 对无效描述符 12345 执行 F_GETFL 仍返回 0。
- F_SETFL(fd, O_APPEND) 返回 0；seek 到开头后写 Z，原文件首字节被覆盖成 Z，未追加。

建议：实现支持的读取/设置命令，访问模式与状态标志分开保存；不支持的命令返回错误，F_DUPFD 按下限分配。与上一轮 open 时 O_APPEND 透传修复并不冲突，本问题发生于打开后的 F_SETFL。

### B28 [P1] load 自动压实会丢掉尚未发往服务器的 DELETE

位置：`shared/sync/store.rs` 的 `load` / `compact`，以及 `shared/sync/sync.rs` 的删除推送循环。

宿主直接引用当前源复现：建立 FLAG_ON_SERVER 记录，remove 后得到 DELETED|DIRTY，save 成功；重新 load 时，墓碑占比超过 25% 触发 compact。compact 不区分已确认删除和待同步删除，移除全部墓碑；**记录数变为 0，待同步队列也变为 0**。

结果：删除操作在重启后消失，服务器仍保留旧事件/联系人，后续拉取可能再次显示。保存索引偏移修复不能解决这个状态机错误。

建议：仅回收服务器已确认删除、且无需继续参与冲突处理的墓碑；DIRTY 的远端删除意图必须持久化到成功响应之后。

### B29 [P1] upsert 容量不足返回失败，却修改了存储状态

位置：`shared/sync/store.rs` 的 `upsert` / `append_blob`。

两项独立宿主用例确认：

- 新建：blob 缓冲区仅 2 字节，插入更长内容返回 false，但 n 已从 0 变 1，find 能找到空记录，且带 DIRTY。
- 更新：已有 old 内容，更新更长内容失败，但 mtime、coll、DIRTY 和修订号已经变更，内容却仍是 old。

建议：容量与记录合法性先验证，再统一提交元数据与 blob；失败保持原状态。否则调用方重试、保存或同步时会看到半完成记录。

## 其他确认的问题

### B30 [P2] readv 把实际读取错误伪装成 EOF

位置：`kunxcore/src/linux_compat.rs` 的 `sys_readv`，`if r <= 0 { break; }`。

来宾对 O_WRONLY 文件执行普通 read 得到负错误，但同样的 readv 返回 **0**。调用方会把权限/描述符错误误判为文件结束。

建议：r<0 时按是否已有进度返回错误或已完成字节；只有 r==0 才作为 EOF。

### B31 [P2] 管道不检查读端/写端权限

位置：`kunxcore/src/ipc.rs` 的 `pipe_read` / `pipe_write`，以及 linux_compat 的对应分派。两者检查 FdKind::Pipe，却没有检查 FD_READ/FD_WRITE。

来宾创建管道后，向 **读端** write Q 返回 1，再从 **写端** read 得到 Q，返回 1。端点权限未生效。

建议：在底层操作统一验证端点访问权限并传递 EBADF；不能只在 poll 的就绪查询里区分两端。

### B32 [P2] 同 key 的 upsert 无法恢复已删除记录

位置：`shared/sync/store.rs` 的 `upsert`。find_any 命中墓碑后保留旧 flags，没有去掉 DELETED。

宿主复现：创建 a→删除 a→upsert 同 key 的 new，upsert **返回 true**，但 find(a) 仍为 None，记录仍被标记删除。本问题描述的是同 key 恢复，不代表 UI 每次新建生成新 UID 都会触发。远端冲突处理中有局部清标志逻辑，未让通用 Store API 本身满足这一契约。

建议：明确“恢复/覆盖墓碑”与“更新墓碑元数据”的 API，成功写入新实体时按规则重置删除状态。

### B33 [P2] load 接受损坏的 blob，却把 loaded 标为成功

位置：`shared/sync/store.rs` 的 `load`，读取 blob 后没有逐条验证有效记录的 boff+blen。

宿主复现：先成功保存带正文的记录，再把 blob 截成 1 字节；load 返回 true、loaded=true、find 仍有该记录，但 blob_of 返回 None。缺失 blob 也走相似路径。

建议：校验所有有效记录范围，区分不存在的空存储、I/O 失败和损坏；错误时不要把新状态标为完整可用。这是上一轮“两文件保存不具备掉电原子性”之外的加载校验缺失，不宣称增加校验就能解决原子性。

## 修复前复现证据（历史记录）

- `python docs/audit-2026-11-15-repro.py`：**5 个审计专用用例均重现错误**，引用真实 shared/sync 与 MemFs；49 个引入模块原有用例被过滤。本脚本成功表示问题重现，不表示修复通过。
- 来宾命令：`python tools/test_audit_guest.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --build-dir build/audit-2026-11-15/guest --source tests/audit_second_guest.rs --success-marker "AUDIT2 REPRO COMPLETE"`。
- 来宾最终输出 `AUDIT2 REPRO COMPLETE`，包含 7 条 REPRO 观测：writev、readv、fcntl 基本契约、fcntl 追加覆盖、readv 错误吞掉、只读截断、管道端点。它们归并为上方 5 个内核问题，不能按日志行数当作 7 个独立漏洞。
- 来宾测试实际重建并链接当前内核，在独立 staging rootfs/ramdisk 内运行，仅破坏测试镜像里的 `/tmp/audit2-vectors`。没有修改用户默认镜像/文件数据。
- 本地证据：`build/audit-2026-11-15-host.log`、`build/audit-2026-11-15-guest.log`、`build/audit-2026-11-15/guest/serial.log`。build 子目录只保留 .gitignore 进入版本管理，构建物不应提交。

上述结果描述修复前状态，当前验证请使用修复记录中的命令。上一轮已知但未完成的权限隔离、加密/TLS、信号/共享映射与动画问题仍见 `repair-status-2026-11-14.md`，没有在此重复计数。
