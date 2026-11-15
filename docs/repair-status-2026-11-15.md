# 第二轮修复记录（2026-11-15）

本次修复 [补充审计](project-audit-2026-11-15.md) 的 B25–B33。工作区同时有其他会话修改，以下仅归属本次实现与验证，不代表整个项目已无 bug。

| 编号 | 修复 | 正向回归 |
| --- | --- | --- |
| B25 | readv/writev 快照 iovec，逐项逐块处理，错误或短传输返回连续前缀，不跨过当前元素 | 8192+4 字节向量、EOF、空元素、部分完成后坏指针 |
| B26 | VFS 截断先验证写访问权限，只读返回 EBADF | 只读截断失败且长度保留，可写截断成功 |
| B27 | F_DUPFD 按下限分配；GETFL/SETFL 读取与更新共享打开文件状态；无效 fd 与不支持命令返回错误 | 最小 fd、非法下限、访问模式、共享追加标志及实际追加内容 |
| B28 | load 不再触发写入/压实；compact 保留待同步或冲突的远端删除；服务器确认后清理待发送状态，墓碑保持隐藏 | 删除后保存/重启/压实，端到端远端删除只发送一次 |
| B29 | upsert 在修改计数、元数据和正文前验证长度及容量 | 新建/更新失败均保留原状态，可重试 |
| B30 | readv 区分错误和 EOF，有进度时返回进度，无进度时返回原错误 | 只写描述符上的 readv 返回错误 |
| B31 | 管道底层读写统一检查端点权限，错误以有符号值传递 | 普通及向量读写拒绝错误端点，正确端点可传输 |
| B32 | upsert 替换实体时清除 DELETED；空正文明确替换旧内容 | 同 key 重建可见、正文正确，空替换生效 |
| B33 | load 验证有效正文范围，拒绝缺失/截断及溢出范围；失败不暴露部分记录或 loaded=true | 缺失/截断 blob 被拒绝 |

实现涉及 `kunxcore/src/{linux_compat,process,vfs,ipc,kos_vfs}.rs` 和 `shared/sync/{store,sync}.rs`。F_DUPFD 复用带下限的 fd 分配函数；KOS fd 所有权检查区分“不支持”和“无效 fd”。同步删除确认同时修正了旧的清除 DELETED 导致空记录重新可见的问题；纯本地删除不再进入远端发送队列。

## 已完成验证

- `cargo test --manifest-path sync_tests/Cargo.toml`：68 项通过，包含新增状态回归和重启后远端删除集成用例。
- `python docs/audit-2026-11-15-repro.py`：7 项正向存储回归通过。文件名沿用审计入口，成功现在表示修复验证通过。
- `python docs/audit-2026-11-14-repro.py`：上一轮 15 项宿主内核回归通过。
- `python tools/test_audit_guest.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --build-dir build/audit-2026-11-15/fix-guest --source tests/audit_second_guest.rs --success-marker "AUDIT2 REGRESSION PASS"`：实际 RISC-V release 内核构建并启动，通过 `AUDIT2 REGRESSION PASS`。
- 同一来宾运行器使用默认源码与 `build/audit-2026-11-15/previous-guest`：上一轮 `AUDIT GUEST PASS` 通过。
- `python tools/test_display.py --build-only --all-apps --build-dir build/windows`：重建所有脚本覆盖的自适应应用及 BAT 默认内核成功。
- `python tools/test_display.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --no-build --build-dir build/windows --animate --resize --host-cursor --all-apps`：PASS。覆盖桌面启动、共享 ELF 页、设置首帧与打开/返回、竖横屏切换、电话应用缩放、宿主光标和无残影检查；鼠标与 Dock 操作没有重建显示表面。设置/电话启动动画记录为 17/17/17/14 帧，返回为 14/14/10/5 帧。

本地证据保存在 `build/audit-2026-11-15/` 下的 `fix-sync.log`、`fix-host.log`、`previous-host-regression.log`、`fix-guest-run.log`、`previous-guest-run.log`、`windows-build.log`、`display-run.log`，以及 `build/windows/report.json`。构建物与日志不应提交。

## Windows 启动

项目根目录新增 `启动KunxOS.bat`。双击打开 QEMU GTK GUI；已有默认内核时直接使用，缺少时自动构建。已试运行并确认窗口及桌面加载。修改源码后使用 `启动KunxOS.bat --build` 主动更新；本次默认镜像已更新到本轮修复。详情见 [显示与启动说明](display-adaptation.md)。

## 保留的限制

`fcntl` 尚不完整：CLOEXEC、KOS fd 操作及管道非阻塞状态等明确返回不支持；普通文件状态位支持不代表完整 Linux ABI。O_APPEND 后的当前位置语义也尚未完整验收。同步索引与 blob 两文件保存仍不具备掉电原子性。本轮没有解决上一轮记录的普通 VFS 身份/模式隔离、旧加密与 DAV TLS、信号/共享映射或全部缺失动画，也没有做真机验收。

工作区存在其他会话修改，最终全局 `git diff --check` 发现其他测试脚本的行尾空白；本次修改文件单独检查，不清理其他会话的改动。
