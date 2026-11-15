# 四大目标路线图与现状（2026-11-15）

目标：全部自研、迅速、稳定、高度可定制。每项只记录已核实的事实；"完成"必须有可重复的验证命令。

## 本轮核实结果

| 项 | 结果 |
| --- | --- |
| 内核 `cargo check --release`（riscv64gc） | 通过 |
| `cargo test --manifest-path sync_tests/Cargo.toml` | 73 项通过 |
| `python docs/audit-2026-11-14-repro.py` | 15 项通过 |
| 来宾 `tests/fcntl_guest.rs` | `FCNTL REGRESSION PASS`（此前文档称"尚未编译或运行"，现已实测） |
| 来宾 `tests/audit_second_guest.rs` | `AUDIT2 REGRESSION PASS` |
| 来宾 `tests/vfs_perm_guest.rs`（新增） | `VFSPERM REGRESSION PASS` |
| `tools/test_display.py … --all-apps` | PASS（QEMU ramfb，非真机） |

`BUG_REPORT.md` 的 C-1/C-2/C-3 已过时：`USER_TOP` 已是 `0x4000_0000`，所列系统调用已走
`user_ok_writable` 校验，ELF 与 mmap 已有用户地址上界检查。该文件不应再当作当前缺陷清单。

## 本轮修复

- `sys_renameat` 把所有 VFS 失败都映射成 `EEXIST`，其他用户的带归属文件被拒绝改名时应用看到的是
  "已存在"而不是 `EACCES`。现在 `-13` 原样返回。回归：`tests/vfs_perm_guest.rs`。
- 新增 VFS 身份/权限来宾回归（此前 `completion-2026-11-15.md` 标注"尚无自动测试"）：
  属主创建 0600 文件、其他 uid 的 read/write/rdwr/unlink/rename/chmod 均被拒、root 可读、属主可删除。

运行：

```
python tools/test_audit_guest.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe" --build-dir build/vfsperm-guest --source tests/vfs_perm_guest.rs --success-marker "VFSPERM REGRESSION PASS"
```

## 路线

### 1. 全部自研

先定边界：运行时（设备上跑的代码）与工具链（构建机）分开验收。

1. 统一用户态运行库：收敛入口、syscall、分配器（`self-developed-stack.md` 第 3 层）。
2. 评估 `core`/`alloc` 之外的编译器内建依赖，列出必须保留的最小清单（第 4 层）。
3. 自研 M 模式固件替代 OpenSBI（第 5 层，工作量大，建议最后做）。
4. 决定 NetSurf（174MB，`vendor/netsurf`）是否属于系统：属于则需自研浏览器内核，否则移出发行内容并在文档中声明为可选第三方应用。
5. 字体与图标资产：改为自绘或明确授权来源。

### 2. 迅速

当前只有 QEMU TCG 下的显示基线，没有可用于"特别快"判断的数据。

1. 新增启动时间基线：从 `_start` 到桌面首帧，串口打点，写入 `perf-baseline.md`。
2. 系统调用延迟（getpid/read/write/pipe 往返）与 SFS 顺序读写吞吐基线。
3. 在 KVM 或真机（先 RISC-V 板）上复测显示基线；240Hz 目标在拿到实测前不得写成"已达成"。
4. 1080p 整屏纯 CPU 合成余量仅约 1.3ms，优先做脏区合并与降级路径的实测。

### 3. 稳定

1. 长稳测试：循环启动/操作/退出上百轮，复现 `DEBUG_NOTES.md` 的偶发 `PC=0 el0_fault`。
2. 剩余已知缺口：`faccessat` 带 mode 返回 `ENOSYS`（VFS 已有 ACL，可接上）、目录逐级 x 位不检查、
   execve 不检查 x 位、tmpfs/FAT32 不保存归属、同步存储 FNV 校验只防撕裂写。
3. 相机环形帧写入检查（见 `KUNXOS_RUN_REPORT.md` 第 2 项）。
4. 把上述来宾回归接入 CI（`.github/workflows/ci.yml`），目前只有手动命令。

### 4. 高度可定制

目前只有壁纸、导航方式、锁屏方式、分辨率自适应。

1. 先写"可定制点清单"：主题（颜色/圆角/字体）、启动项（`/etc/inittab`）、内核参数、应用权限策略。
2. 把现有硬编码主色（`#5B63D9` 等）收敛到一个运行时可读的主题配置文件，设置应用可改。
3. 给 `kunxpkg` 增加可安装的主题/配置包类型，并在 `kunx-package-format.md` 中说明。

## 不在本轮范围

真机验证、自研固件、浏览器替换、长稳测试均未做；以上四条路线的各项尚未开始，除"本轮修复"一节外没有新功能。
