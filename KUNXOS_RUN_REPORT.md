# KunxOS 运行检查报告(2026-11-15)

## 运行环境说明

没有使用你电脑上的 WSL。我这边无法调用 wsl.exe,所以按你的选择改在独立的 Linux 沙箱里运行:把项目复制到沙箱,安装 Rust nightly(riscv64gc-unknown-none-elf)、QEMU 6.2(riscv64 virt),用 zig cc 代替 clang。你桌面上的 kunxos 目录里的源码没有被修改,本报告是唯一新增文件。

与你本机环境的差异会影响结论,请注意:

1. QEMU 是 6.2,不支持 `virtio-sound-device`(需要 8.2 及以上)。我在包装脚本里把音频设备参数去掉了,因此音频相关路径没有被真实测到。
2. 复制时跳过了 `tools/` 下的大型交叉编译器、`vendor/`(netsurf)、`third_party/`、`build/` 和 `*.img`。这些没有参与默认内核构建,但依赖它们的目标(busybox、浏览器移植等)没有测。
3. 没有跑图形界面(GUI)回归和需要外网的测试(weather_e2e、tlstest)。

## 发现的 bug

### 1. 内核无法编译:fs_simple.rs 借用冲突(已确认,阻断级)

位置:`kunxcore/src/fs_simple.rs` 第 2164 行。这是尚未提交的改动(git blame 显示今天 14:33 修改)。

```rust
put_u16(&mut inode, I_MODE, if inode_type(&inode) == T_DIR { 0o755 } else { 0o644 });
```

同一次函数调用里先 `&mut inode`、参数里又 `&inode`,报 E0502。显式的 `&mut x` 参数不属于 two-phase borrow,所以任何 Rust 版本都过不了,不是我用新 nightly 造成的。后果是当前工作区的内核根本编不出来。

建议修法(我在沙箱副本里用它编译通过了):

```rust
let default_mode = if inode_type(&inode) == T_DIR { 0o755 } else { 0o644 };
put_u16(&mut inode, I_MODE, default_mode);
```

### 2. 相机预览帧文件为空或被截断,但 latest 序号照常推进(已确认根因)

现象:`tests/scripts/camera_e2e_test.py` 在 "advancing preview and frame header" 一步失败,`od /tmp/cam/frame_1.yuv -n 4` 没有输出。单独复现时,`/tmp/cam` 的 6 个环形帧文件中只有 frame_1(115222 字节)和 frame_2(106496 字节,已被截断)有内容,其余 4 个是 0 字节,而 `latest` 已推进到 28、30 等。所以测试是否通过,取决于序号落在哪个文件,表现为时好时坏。

根因:两处因素叠加。

- `kunxcore/src/tmpfs.rs`:`/tmp` 只有 160 块(640KB),扣掉日志和元数据后约 76 个数据块(约 300KB)。
- `kunxserv/camerad/src/main.rs` 的 `write_ring_frame`:6 个 320x240 YUV420 帧每帧约 115KB,合计约 690KB,装不下。该函数用 `O_TRUNC` 打开后不检查 `write` 的返回值,`open` 失败时直接 `return`,写满(ENOSPC)后仍然更新 `latest`。

后果:相机应用读取预览时会拿到空帧或半帧。

建议:检查 `write` 返回值,失败时不更新 `latest`;同时减小环形缓冲帧数或扩大 tmpfs 容量,二者至少改一个。

## 通过的检查

内核(含修复 #1 之后)构建成功,系统启动到 init,拉起 networkd、wifid、audiod、camerad、lifd、alarmd、sensord、rild、pushd,没有 panic。以下项目自带测试通过:`test_qemu_test.py`(4/4)、`smoke_test.py`、`lifecycle_cmd_burst.py`、`lifecycle_core_test.py`(38/38)、`power_test.py`(21/21)、`debt_diag.py`(10/10)。`camera_e2e_test.py` 前两步通过,第三步失败(见 bug 2)。

## 观察到但不判定为 bug

无图形(无 ramfb)模式下 `/dev/fb0` 打不开,launcher 重启 8 次后 init 放弃并降级为控制台模式,日志有 `[health]` 提示。这是预期的降级行为,没有 panic。另外 `/data/...` 的大量 open 失败是因为我没有挂载持久磁盘,不属于系统问题。

DEBUG_NOTES.md 提到的偶发 PC=0 的 el0_fault,本次有限的几轮运行中没有出现,不能据此说明已经修复。

## 没有覆盖的部分

GUI 端到端回归(apps_regression、lifecycle_test 等)、音频、浏览器、云网关、包管理测试(kunx_pkg_test)、长时间稳定性(DEBUG_NOTES 里的偶发故障需要上百轮)。如果你想继续,在你本机的 WSL 里装 QEMU 8.2 及以上后,这些可以直接用 `tests/scripts/README.md` 里的命令跑。
