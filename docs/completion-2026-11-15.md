# 半成品收尾记录（2026-11-15）

本轮完成四项，**全部尚未编译或运行**（编写环境没有 cargo / QEMU）。请按下面顺序在本机验证，失败信息贴回即可继续修。

## 1. fcntl 补全
- `process.rs`：每进程 `cloexec` 位图；fork 继承，fd 释放/重分配/dup3 目标清零；新增 `fd_cloexec / set_fd_cloexec / set_fd_flags / take_cloexec_mask`。
- `linux_compat.rs`：F_GETFD/F_SETFD、F_DUPFD_CLOEXEC、open/pipe2/dup3 的 O_CLOEXEC、execve 成功后关闭带标志的 fd；pipe2 与 F_SETFL 支持 O_NONBLOCK；pipe2/dup3 拒绝未知标志（EINVAL）。
- `ipc.rs`：`FD_NONBLOCK`，管道空读/写满返回 EAGAIN（有进度则返回进度）。
- `kos_vfs.rs`：KOS 桥接 fd 支持 F_GETFD/F_SETFD/F_GETFL/F_SETFL(O_APPEND) 与 O_CLOEXEC；F_DUPFD 仍返回 ENOSYS。
- `vfs.rs`：O_APPEND 写完后偏移置为文件末尾。
- 已知限制：管道 O_NONBLOCK 按 fd 保存（Linux 为按打开文件描述保存，dup 后不共享）；Pair 的非阻塞未实现。
- 测试：`tests/fcntl_guest.rs`

## 2. 同步存储掉电安全
- `shared/sync/store.rs`：代际文件 `<idx>.N` + `<blob>.N`，索引头带 Blob 长度/校验和与自身校验和，索引写完即提交；加载选最新有效一代，损坏则回退；旧单文件首次保存后迁移并删除；保存失败不改内存。
- `util.rs`：增量 `fnv1a_update`。
- 测试：`sync_tests/src/storage_power_loss.rs`（每个写步骤中断 + 半写）；`storage_state_regression.rs` 中按名字破坏 blob 的用例已改为使用 `active_slot()`。
- 校验和为 FNV-1a 32 位，用于检测撕裂写，不防恶意篡改。

## 3. VFS 身份与权限
- 策略：只拦截有归属的文件；root 与无归属文件行为不变。
- `fs_simple.rs`：inode `@132..143` 存 uid/gid/mode/标志，非 root 创建时在创建事务内盖章；`fs_owner / fs_set_owner`。`fsops.rs` 分发（FAT32 无归属）。
- `vfs.rs`：open/创建/unlink/rename/mkdir/mknod/symlink 执法，返回 -13；chmod/chown 底层。
- `linux_compat.rs`：新增 fchmod/fchmodat/fchown/fchownat，stat 返回真实 uid/gid/mode。
- 已知限制：不逐级检查目录 x 位；execve 不检查 x 位；tmpfs/FAT32 不保存归属；新建文件默认 0644，应用需自行 chmod 收紧。
- 文档：`docs/sfs-v2.md` inode 布局已更新。
- 尚无自动测试：建议补一个来宾用例（setuid 后创建文件，换 uid 后读写/unlink 应得 -13）。

## 4. dead_code
- 全局 `allow(dead_code)` 改为 `cfg_attr(not(feature = "strict-dead-code"), ...)`，默认构建不变。
- `tools/dead_code_report.sh` 汇总清单，之后逐项处理。未直接删除，因为无编译器无法保证 CI 的 warning 门槛不被打破。

## 验证命令（Windows 本机）
```
cargo test --manifest-path sync_tests/Cargo.toml
python docs/audit-2026-11-15-repro.py
python docs/audit-2026-11-14-repro.py
cargo check --release --manifest-path kunxcore/Cargo.toml --target riscv64gc-unknown-none-elf
python tools/test_audit_guest.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --build-dir build/fcntl-guest --source tests/fcntl_guest.rs --success-marker "FCNTL REGRESSION PASS"
python tools/test_audit_guest.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --build-dir build/audit-2026-11-15/guest --source tests/audit_second_guest.rs --success-marker "AUDIT2 REGRESSION PASS"
python tools/test_display.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --build-dir build/windows --animate --resize --host-cursor --all-apps
```
最后一条用来确认桌面和各应用没有因权限检查或 pipe2 标志校验而回归。
