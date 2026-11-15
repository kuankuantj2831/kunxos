# RISC-V 测试入口

测试目标统一为 `riscv64gc-unknown-none-elf`，QEMU 使用 `virt` / `rv64` / OpenSBI，默认 TCG。宿主 Rust 测试仍在宿主执行，不应改成裸机目标。

## 快速回归（Windows / Linux）

先安装 Rust 的 RISC-V target 与 `qemu-system-riscv64`，在仓库根目录运行：

```sh
python tests/scripts/test_qemu_test.py
python tests/scripts/smoke_test.py --build-dir build/test-riscv
python tests/scripts/browser_test.py --build-dir build/test-riscv
```

构建器在 `build/test-riscv` 下创建独立 rootfs、ramdisk 和 Cargo 输出，重建 init、所需测试应用与内核，不覆盖日常启动镜像。`smoke_test` 检查用户态 ecall、hello 和 syscall probe；浏览器测试验证本地文件与宿主 HTTP 测试页的内容，不依赖公网。日志保留在构建目录。CI 执行以上三个入口。

`qemu_test.py` 为来宾测试共享配置和串口实现：优先使用 `QEMU` 环境变量，然后查 PATH、Windows 默认安装路径和原有用户目录安装。`BIOS`（或 `OPENBI`）可指定 OpenSBI 固件，默认 `default` 使用 QEMU 自带固件。`QEMU_ACCEL` 默认 `tcg`。不要在 x86 宿主对 RISC-V 强制使用 KVM。

## 已构建镜像的功能回归

`KERNEL` 环境变量可指定 RISC-V ELF 或 raw kernel image；默认是仓库的 `kunxcore/kernel.img`。PowerShell 示例：

```powershell
$env:KERNEL = (Resolve-Path build/test-riscv/target/riscv64gc-unknown-none-elf/release/rust-kernel).Path
python tests/scripts/power_test.py
python tests/scripts/lifecycle_cmd_burst.py
python tests/scripts/lifecycle_core_test.py
python tests/scripts/lifecycle_test.py
python tests/scripts/debt_diag.py
python tests/scripts/camera_e2e_test.py
```

Linux 使用 `export KERNEL=/absolute/path/to/rust-kernel`。日志位置可用各脚本的位置参数指定，缺省位于宿主临时目录；QEMU 标准错误另存为同名 `.qemu-error.log`。串口与 monitor 通过 localhost TCP 连接，在 QEMU 启动失败时有超时与错误日志；无需 Unix FIFO。

GUI 回归应使用已完成 OOBE 且未锁屏的 540×960 测试镜像。可先运行：

```sh
python tools/test_display.py --build-only --all-apps --build-dir build/test-riscv/gui
```

然后将 `KERNEL` 指向 `build/test-riscv/gui/target/riscv64gc-unknown-none-elf/release/rust-kernel`。运行 `apps_regression.py`、`verify_files_app.py` 或 `app_fixes_verify.py`。launcher 会报告真实的 tile、dock、folder 和 folder-app 坐标，测试按日志定位；镜像中的 launcher 也必须重新编译。转场、缩放、鼠标和刷新率验证使用 `tools/test_display.py --animate --resize`。

可用 `python tests/scripts/apps_regression.py --only clock files` 仅巡检指定应用；缺少的应用报告跳过，不计为通过。

部分套件另需环境：

- `app_fixes_verify.py`：镜像需预置 store 的 mock 源配置（`http://10.0.2.2:18085/test`）和 cloud 的 mock 网关（`http://10.0.2.2:8080`）。脚本启动本地 mock 服务。
- `cloud_e2e.py`：先在专用测试目录运行 `cloud/gateway/gateway.py --local --port 8080 --data <测试数据目录> --admin-key admin`；默认使用 `http://127.0.0.1:8080`，可用 `CLOUD_GATEWAY` 覆盖。此测试会下发 lock/wipe，只连接测试网关。
- `weather_e2e.py` 和 `tlstest.py`：依赖外部网络。TLS 默认不挂载持久磁盘；显式设置 `DISK` 时使用 QEMU snapshot，保护源磁盘。
- `kunx_pkg_test.py`：需要重建 demo 软件源，确保包中 ELF 也是 RISC-V（`make rootfs` 会生成）。
- `devtool_verify.py`：连接 `make run-input` 启动的串口 TCP，支持 `--port`、`--timeout`，失败时返回非零。

## 清理与替代

删除了 `app_open_shots.py`、`app_ui_probe.py`、`perf_harness.py`、`launcher_test.py`、`run_modemtest.sh` 五个重复或过时入口。截图/应用巡检由 `apps_regression.py` 覆盖，转场/帧统计由 `tools/test_display.py` 覆盖，调制解调器回归使用 `make test-modem`。历史报告中的旧文件名只作为当时记录保留。

移除了 rootfs 中没有构建入口的旧 ARM64 `echoline`、`fbtest`、`pthread_test`，以及无引用的 `kunxcore/apps/hello.elf`。`rootfs/bin/hello` 由 `tests/hello.rs` 与现有汇编源构建，`probe` 由 `apps/probe` 构建；顶层 Makefile 会更新这两个来宾测试程序。宿主检查会拒绝 rootfs 中的非 RISC-V ELF。

## 本次本机验证（2026-11-15）

Windows 原生 QEMU / OpenSBI 已验证：RISC-V 内核与 probe/browser/launcher 编译、用户态 hello/probe 冒烟、浏览器文件与 HTTP 回归、电源 21/21、生命周期核心 38/38、完整生命周期 52/52、命令连发、PLIC/网卡诊断 10/10、文件管理器 GUI 打开/截图/退出。宿主 harness 检查 4/4，Python 语法检查与 WSL Make 构建预演通过。

相机端到端验证通过：枚举与 3A 响应、非空 JPEG 与 SOI、递增预览序号与 KXFR 帧头、录像与 KXRV 容器头。GUI 巡检中 16 个应用启动和返回成功，时钟修正日志匹配后单独复跑通过；音乐因镜像缺少入口跳过，未计为通过。

公网 TLS、天气以及需要专用网关或软件源预置的功能套件需按上述前置环境另行执行；不能据架构参数迁移推断这些外部服务的测试结果。
