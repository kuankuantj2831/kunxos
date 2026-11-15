# 2026-11-14 审计问题修复记录

本记录对应 `project-audit-2026-11-14.md` 的 B01–B24。已经修改实现并运行回归，但没有消除全部问题，也没有完成 Linux / Android 兼容。原审计报告保存的是修复前证据。

## 已落实的修改

| 项目 | 当前结果与边界 |
| --- | --- |
| B02/B03 页权限 | 重建数据页权限时清除 X；系统调用输出拒绝只读 ELF / 只读 mmap / PROT_NONE，仅对原本可写的 COW 页复制。增加只读页进入共享引用计数的处理，避免 fork 子进程退出后释放父页。 |
| 新发现：Sv39 漫游与取消映射 | 修正中间表项与叶子判断相反的问题。映射时无条件执行地址级 sfence，包括从无效 PTE 恢复的情况。 |
| B04/B05 同步数据 | SysFs/HostFs 的偏移写不再截断整个文件；Store 只截断一次，索引序列化最终紧凑偏移，blob 完成后发布索引；先检查旧范围，拒绝整数溢出和越界。共享索引暂存区加互斥，load 自动 compact 前释放锁。**两文件保存仍不是掉电原子事务，I/O 失败中途恢复仍需设计。** |
| B06 旧包安装 | NOVAPKG1 的公开 FNV 校验和仍可验证归档损坏，但安装入口固定返回 EOPNOTSUPP，不再写入可执行文件；shell 说明改用 `/bin/pkg` 的 Ed25519 包流程。顺带修正校验和种子乘法在调试模式溢出。未改应用商店现有签名包安装链。 |
| B11 时钟睡眠 | clock_nanosleep 单独按 clockid/flags/req ABI 读取参数；支持 realtime/monotonic 与相对/绝对时间，校验 timespec 并向上取整微秒。nanosleep 共用校验。信号中断/剩余时间语义仍待完善。 |
| B12 execveat | 正确提取 a1/a2/a3 的 path/argv/envp；支持 flags=0 的绝对路径与 AT_FDCWD 相对路径，execve 按当前 cwd 解析；失败恢复原参数帧，成功保留新入口帧。其他 dirfd / AT_EMPTY_PATH 等返回 ENOSYS。 |
| B13/B17/B18 轮询 | pselect 从 a4 读取超时，保留原请求集合直到返回，处理 readfds/writefds；ppoll 清理负 fd 的 revents，零 fd 仍等待超时，拒绝数量超限。持续检查直到就绪或截止，补管道/套接字对、原始网络 socket 的就绪查询。真实信号掩码请求返回 ENOSYS；NULL mask 的 libc 描述符受支持。 |
| B14/B15/B16 mmap | NOREPLACE 检查任意重叠，包含 lazy/PROT_NONE；匿名与普通文件映射遵守请求的只读/无访问权限；PROT_NONE 撤销 PTE 并保留内容、所有权和预留，可恢复。普通文件固定映射及不支持的 framebuffer 权限明确拒绝。共享映射完整 munmap/mprotect 语义仍未补齐。 |
| B19 Windows 构建 | rild 的 Unix PermissionsExt 改为条件编译，Windows 检查 PATHEXT；RISC-V 编译器也支持 KUNXOS_CC，追踪工具环境变化。Windows build.rs 本身编译通过，完整构建仍缺 clang。 |
| B20 增量依赖 | Make 的 SHARED_SRC 收集整个 shared 树，motion/glass/l10n/sync 与其他间接包含文件改变都会触发应用重编。 |
| B21 路径契约 | open/mkdir/unlink/mknod/access/readlink/stat/rename/symlink 的相关 *at 入口拒绝不支持的相对 dirfd，避免错误操作 cwd 下同名文件；faccessat 的非零访问模式返回 ENOSYS，未实现实际 DAC。 |
| B22 排他创建 | 普通 VFS 已有目标或悬空最终符号链接时，O_CREAT|O_EXCL 返回 EEXIST，且发生在 O_TRUNC 前；标志从 syscall 透传。**尚未把存在性检查与创建收进同一不可抢占事务，不宣称并发排他创建完整。KOS 路径另行实现。** |
| B23 字符串 | read_cstr 返回 Result，必须读到 NUL；读失败报 EFAULT、超限报 ENAMETOOLONG，不再使用有效前缀。exec argv/envp 同样拒绝读取错误或超限。KOS 路径辅助仍将错误统一为 EFAULT，但不会操作截断路径。 |
| 附加契约修复 | truncate 不再创建不存在的文件；更新 probe 的测试准备步骤。SysFs delete_file 返回实际 unlink 结果。修复 probe/sleepy 的过期依赖锁文件。 |
| 回归入口 | CI 补 sync_tests、kunxsec、字体宿主测试与实际运行型 l10n/weather/SFS；执行内核逻辑回归，编译来宾测试程序。QEMU 来宾回归可在本地单独运行，未在 CI 自动启动。 |

## 验证结果

- 53 个裸机 crate 执行 `cargo check --offline --locked --target riscv64gc-unknown-none-elf`：**52 通过**。唯一未完成的是 rild；Windows build.rs 已能编译，执行时缺少 clang。未安装工具或伪造 C 编译结果。日志：本地 `build/audit-2026-11-14/fix-crates/`。
- `cargo test --manifest-path sync_tests/Cargo.toml --offline`：**60 通过**，含真实 HostFs 偏移写、多记录更新保存重载、非法范围保留磁盘内容的新回归。
- `python docs/audit-2026-11-14-repro.py`：**15 通过**。直接抽取当前内核函数，验证时间 ABI、完整超时、pselect 集合、负 fd、权限拒写、Sv39 漫游/取消映射、NOREPLACE、字符串和旧包禁装。调度/设备是宿主桩，sfence 指令在宿主测试中被移除；不能单独证明来宾硬件行为。
- `python tools/test_audit_guest.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --build-dir build/audit-2026-11-14/fix-guest`：**AUDIT GUEST PASS**。真实 RISC-V 验证 >50ms 睡眠、零 fd ppoll、socketpair/select 就绪、部分 lazy NOREPLACE 冲突、PROT_NONE 拒写与恢复内容、只读页子进程写故障、引用生命周期、取消映射再预留、代码改 RW 后执行故障、ELF 只读输出拒绝、O_EXCL、execveat 相对 cwd 实际执行 busybox true。
- 隔离桌面 `test_display.py --all-apps --animate --resize --host-cursor`：**PASS**，重建相关应用和内核，启动/返回、分辨率切换、光标路径通过。配置 144Hz，`hardware_vsync=false`，不能宣称稳定实测 144fps。此次启动帧为 15/15/17/17，返回帧为 15/16/16/13。
- Python 语法检查与 `git diff --check` 通过。CI 配置已补充，本地测试通过不等于远端工作流已执行成功。

来宾和桌面测试均使用新建 staging rootfs、独立 ramdisk/target；没有替换默认 rootfs 和启动镜像。仓库在工作期间有并行修改/提交，本记录按所改模块和验证结果归因，不将当前全部分支变更归于本轮。

## 仍需完成

1. **B01 普通 VFS 的 owner/mode/UID 强制访问控制**：需要持久化元数据、统一凭据与所有文件操作入口执法，当前未实现。
2. **B07/B08/B09/B10**：磁盘 XOR、账户秘密 XOR、锁屏 PIN 的无盐 FNV、DAV Basic 明文传输尚未替换。需要数据迁移、密钥来源及 TLS 传输后端，不能靠改名称成为可靠保护。
3. **B17/B21/B22/B24 的剩余兼容性**：信号与轮询协同、设备特定就绪、完整 dirfd/access、并发 O_EXCL、TCGETS/sigmask/altstack/mincore 等成功桩仍有欠缺。网络原始句柄与进程 fd 的命名空间也未统一，碰撞/跨进程隔离不能靠本次就绪查询解决。
4. **持久化与映射边界**：同步两文件掉电一致性、共享 framebuffer/SHM 的完整权限和生命周期、mmap 地址分配碰撞后的择址策略需要继续完善。
5. **动画缺项仍未补完**：全场景可打断并继承速度、设置页非阻塞调度、统一减少动态效果、更多应用页面/控件、实时活动、AOD 与真实呈现统计。桌面回归通过只证明现有路径未退化，不代表这些动画已实现。
6. **Linux / Android 17 差异仍成立**：没有添加 ART、Android Framework、Binder、CTS/VTS 或普遍 Linux 软件兼容层。Android 17 专属特性仍未完成官方在线核验。

因此本轮应表述为“关键数据、页权限与部分系统调用问题已修复并回归”，不能表述为“所有 bug、无用代码和动画缺项全部解决”。
